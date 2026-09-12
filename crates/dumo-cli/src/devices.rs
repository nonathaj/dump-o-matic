//! The storage-device path: list devices, catalogue what is on one, pull content off it.
//!
//! This is the disc pipeline's counterpart for media that is a filesystem rather than a
//! disc. `devices` is to `drives` what `catalog` is to `probe` and `pull` is to `rip`, and
//! the output of `pull` is an ordinary job — so `identify`, `migrate` and `verify` work on
//! it afterwards without knowing where it came from.
//!
//! Read-only with respect to the device, always. The device is opened `O_RDONLY` and its
//! filesystem is parsed in-process rather than mounted, so nothing here can write to the
//! medium even by accident. The only things written are inside the staging root.

use anyhow::{bail, Context, Result};
use dumo_core::config::{self, Config};
use dumo_core::job::{self, Artifact, DeviceSource, Job, JobStage};
use dumo_core::hash;
use dumo_devices::{
    god::GodImage, iso, layout, xcontent, xdvdfs, xex, Catalogue, CatalogueItem, ContentSource,
    DirSource, Fat32Source,
};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::rip::{confirm, human_size, preflight_space};

/// How much of an executable to read when cross-checking its identity. Three of the eight
/// packages measured keep the execution-info block past 8 KB, so this is generous.
const XEX_PREFIX: u64 = 1 << 20;

/// What `pull` should write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PullFormat {
    /// A single `.iso`, converted for ordinary tools and emulators.
    Iso,
    /// The package exactly as the console wrote it, hash tree and all.
    Package,
    /// Both of the above.
    Both,
}

impl PullFormat {
    fn wants_iso(self) -> bool {
        matches!(self, PullFormat::Iso | PullFormat::Both)
    }
    fn wants_package(self) -> bool {
        matches!(self, PullFormat::Package | PullFormat::Both)
    }
}

pub struct CatalogArgs {
    pub device: Option<String>,
    pub json: bool,
}

pub struct PullArgs {
    pub device: Option<String>,
    pub title: Option<String>,
    pub format: PullFormat,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

/// `devices`: list attached storage that might hold console content.
pub fn list(json: bool) -> Result<()> {
    let devices = dumo_devices::enumerate_devices().context("enumerating storage devices")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }
    if devices.is_empty() {
        println!("No storage devices found.");
        return Ok(());
    }
    for d in &devices {
        println!("{}  {}", d.path, d.description());
        println!(
            "    size:   {}{}{}",
            human_size(d.size_bytes),
            if d.removable { ", removable" } else { "" },
            d.transport
                .as_ref()
                .map(|t| format!(", {t}"))
                .unwrap_or_default()
        );
        if !d.readable {
            println!(
                "    access: cannot read this device. Add your user to the 'disk' group \
                 and log back in, or mount it read-only and pass the mount directory"
            );
            continue;
        }
        // Say what is on it, since that is the question the operator actually has.
        match Fat32Source::open(Path::new(&d.path)) {
            Ok(source) => match layout::detect(&source) {
                Ok(c) => {
                    let games = c.games().count();
                    println!(
                        "    content: {} — {} game(s) of {} item(s)",
                        c.layout,
                        games,
                        c.items.len()
                    );
                }
                Err(e) => println!("    content: {e}"),
            },
            // A whole disk that is partitioned has a partition table where a FAT32 boot
            // sector would be, which is the common case here rather than an error worth
            // spelling out in full.
            Err(dumo_devices::DeviceError::NotFat32 { .. }) => {
                println!("    content: no FAT32 volume here (a partitioned disk? try its partition, e.g. {}1)", d.path)
            }
            Err(e) => println!("    content: {e}"),
        }
    }
    Ok(())
}

/// `catalog`: list the content on one device.
pub fn catalog(args: CatalogArgs) -> Result<()> {
    let (source, catalogue) = resolve(args.device.as_deref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&catalogue)?);
        return Ok(());
    }

    println!("{} — {}", source.describe(), catalogue.layout);
    if let Some(medium) = &catalogue.medium {
        println!("  medium: {medium}");
    }
    if catalogue.items.is_empty() {
        println!("  no content found");
        return Ok(());
    }
    println!();
    println!(
        "  {:<9} {:<26} {:<16} {:>10} {:>10}  {}",
        "title id", "name", "kind", "on device", "as iso", "discs"
    );
    for item in &catalogue.items {
        let discs = if item.disc_in_set > 1 {
            format!("{} of {}", item.disc_number, item.disc_in_set)
        } else {
            "-".to_string()
        };
        println!(
            "  {:<9} {:<26} {:<16} {:>10} {:>10}  {}",
            item.title_id,
            truncate(&item.name, 26),
            truncate(&item.kind.to_string(), 16),
            human_size(item.package_bytes),
            if item.extractable {
                human_size(item.iso_bytes())
            } else {
                "-".to_string()
            },
            discs
        );
        if let Some(note) = &item.note {
            println!("      {note}");
        }
    }
    println!();
    println!(
        "  {} of {} item(s) can be pulled off as game images.",
        catalogue.games().count(),
        catalogue.items.len()
    );
    println!("  Pull one with: dump-o-matic pull {} --title <title id>", source.describe());
    Ok(())
}

/// `pull`: extract one title into staging as a job.
pub fn pull(args: PullArgs) -> Result<Option<String>> {
    let cfg = Config::load(args.config_file.as_deref())?;
    let errors: Vec<_> = config::check(&cfg)
        .into_iter()
        .filter(|c| c.level == config::CheckLevel::Error)
        .collect();
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("config error: {} — {}", e.subject, e.detail);
        }
        bail!("configuration is not usable; run 'dump-o-matic config check'");
    }

    let (source, catalogue) = resolve(args.device.as_deref())?;
    let item = select(&catalogue, args.title.as_deref())?;

    if !item.extractable {
        bail!(
            "{} ({}) cannot be pulled off as a game image: {}",
            item.name,
            item.title_id,
            item.note.as_deref().unwrap_or("unsupported content")
        );
    }

    // Parse the package properly now, rather than relying on the catalogue's summary.
    let header_bytes = source
        .read_prefix(&item.path, xcontent::HEADER_SIZE)
        .with_context(|| format!("reading the package header at {}", item.path))?;
    let header = xcontent::XContent::parse(&header_bytes, &item.path)?;
    let image = GodImage::open(source.as_ref(), &item.path, &header)?;

    let iso_plan = if args.format.wants_iso() {
        Some(iso::plan(&image, &header)?)
    } else {
        None
    };

    let mut required = 0u64;
    if let Some(p) = &iso_plan {
        required += p.total_bytes();
    }
    if args.format.wants_package() {
        required += item.package_bytes;
    }

    println!("{} — {}", source.describe(), catalogue.layout);
    println!("  title:     {} ({})", item.name, item.title_id);
    println!("  kind:      {}, {}", item.kind, item.signature);
    println!(
        "  package:   {} across {} data file(s)",
        human_size(item.package_bytes),
        item.data_files
    );
    if let Some(p) = &iso_plan {
        println!(
            "  iso:       {} — {} directory table(s), {} sector reference(s) rewritten by {}",
            human_size(p.total_bytes()),
            p.directories,
            p.references,
            p.shift_sectors
        );
    }
    println!("  to write:  {}", human_size(required));

    if args.dry_run {
        println!();
        println!("Dry run: nothing written.");
        return Ok(None);
    }

    preflight_space(&cfg, required, args.assume_yes)?;

    if !args.assume_yes && !confirm("Pull this title into staging?")? {
        println!("Aborted.");
        return Ok(None);
    }

    // --- A job, exactly as a rip would create one ---
    let job_id = job::new_job_id(Some(&item.name));
    let job_dir = cfg.staging.job_dir(&job_id);
    Job::create_dir(&job_dir)?;
    let raw_dir = cfg.staging.job_raw_dir(&job_id);
    std::fs::create_dir_all(&raw_dir).context("creating the job's raw directory")?;

    let mut jobrec = Job::new(job_id.clone(), source.describe());
    jobrec.stage = JobStage::Ripping;
    jobrec.backend = Some(format!("dumo-devices {}", env!("CARGO_PKG_VERSION")));
    jobrec.save(&job_dir)?;
    println!();
    println!("Job {job_id}");

    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut blocks_verified = 0u64;
    let mut image_sha256 = None;
    let mut base_blocks = 0u64;
    let mut shift_sectors = 0u32;
    let mut image_blocks = 0u64;

    if let Some(plan) = &iso_plan {
        base_blocks = plan.base_blocks;
        shift_sectors = plan.shift_sectors;
        let name = format!("{}.iso", sanitise(&item.name));
        let dest = raw_dir.join(&name);
        println!("  writing {name} ({})", human_size(plan.total_bytes()));

        let report = {
            let file = std::fs::File::create(&dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            let mut out = std::io::BufWriter::with_capacity(4 << 20, file);
            let mut last = 0u8;
            let result = iso::write(&image, plan, &mut out, &mut |done, total| {
                report_progress(&mut last, done, total);
            });
            // Flush before the hash check below reads the file back.
            out.flush().ok();
            result
        };
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                // A partial image must not be left looking like a finished artifact.
                std::fs::remove_file(&dest).ok();
                jobrec.stage = JobStage::Failed;
                jobrec.error = Some(e.to_string());
                jobrec.save(&job_dir)?;
                return Err(e.into());
            }
        };
        println!();
        blocks_verified += report.blocks_verified;
        image_blocks = image.block_count();
        image_sha256 = Some(report.image_sha256.clone());

        // Read the file back and confirm it is what we believe we wrote. The streaming hash
        // covers the bytes handed to the kernel; this covers the bytes that landed.
        print!("  verifying written image ... ");
        std::io::stdout().flush().ok();
        let (disk_sha, bytes) = hash::sha256_file(&dest).context("hashing the written image")?;
        if disk_sha != report.sha256 {
            jobrec.stage = JobStage::Failed;
            jobrec.error = Some(format!(
                "written image hashes {disk_sha} but {} was written",
                report.sha256
            ));
            jobrec.save(&job_dir)?;
            bail!(
                "the image on disk does not match what was written \
                 (wrote {}, read back {disk_sha})",
                report.sha256
            );
        }
        println!("ok");
        println!(
            "  {} blocks verified against the package's hash tree",
            report.blocks_verified
        );

        artifacts.push(Artifact {
            relative_path: format!("raw/{name}"),
            bytes,
            sha256: disk_sha,
            hashed_at: now(),
        });
    }

    if args.format.wants_package() {
        let dir_name = format!("{}.god", sanitise(&item.name));
        let package_dir = raw_dir.join(&dir_name);
        let data_dir = package_dir.join(format!("{}.data", header_name(&item.path)));
        std::fs::create_dir_all(&data_dir).context("creating the package directory")?;
        println!(
            "  copying package verbatim into {dir_name} ({})",
            human_size(item.package_bytes)
        );

        // The header first: it is what names the package and roots its hash tree.
        let header_dest = package_dir.join(header_name(&item.path));
        std::fs::write(&header_dest, &header_bytes)
            .with_context(|| format!("writing {}", header_dest.display()))?;
        artifacts.push(Artifact {
            relative_path: format!("raw/{dir_name}/{}", header_name(&item.path)),
            bytes: header_bytes.len() as u64,
            sha256: hash::sha256_of(&header_bytes),
            hashed_at: now(),
        });

        let mut done = 0u64;
        for index in 0..image.data_files() {
            let name = format!("Data{index:04}");
            let dest = data_dir.join(&name);
            let file = std::fs::File::create(&dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            let mut out = std::io::BufWriter::with_capacity(4 << 20, file);
            let base = done;
            let mut last = 0u8;
            let report = image.copy_data_file_verified(index, &mut out, &mut |written| {
                report_progress(&mut last, base + written, item.package_bytes);
            });
            out.flush().ok();
            let report = match report {
                Ok(r) => r,
                Err(e) => {
                    std::fs::remove_file(&dest).ok();
                    jobrec.stage = JobStage::Failed;
                    jobrec.error = Some(e.to_string());
                    jobrec.save(&job_dir)?;
                    return Err(e.into());
                }
            };
            done += report.bytes;
            blocks_verified += report.blocks_verified;
            artifacts.push(Artifact {
                relative_path: format!("raw/{dir_name}/{}.data/{name}", header_name(&item.path)),
                bytes: report.bytes,
                sha256: report.sha256,
                hashed_at: now(),
            });
        }
        println!();
        println!("  package copied, every block checked against both levels of its hash tree");
    }

    // --- Cross-check the package's claims against the game's own executable ---
    let execution = cross_check(&image, &header, base_blocks, args.format.wants_iso());
    match &execution {
        Some(info) => {
            let agrees = info.title_id == item.title_id && info.media_id == item.media_id;
            println!(
                "  executable: title {} media {} — {}",
                info.title_id,
                info.media_id,
                if agrees {
                    "agrees with the package"
                } else {
                    "DISAGREES with the package"
                }
            );
            if !agrees {
                println!(
                    "      the package says title {} media {}. The extraction is still a \
                     faithful copy, but what it contains is in doubt — do not file it \
                     without looking.",
                    item.title_id, item.media_id
                );
            }
        }
        None => println!("  executable: could not be read for cross-checking"),
    }

    jobrec.artifacts = artifacts;
    jobrec.stage = JobStage::Ripped;
    jobrec.device_source = Some(DeviceSource {
        device: source.describe(),
        layout: format!("{:?}", catalogue.layout).to_lowercase(),
        title_id: item.title_id.clone(),
        name: item.name.clone(),
        media_id: item.media_id.clone(),
        content_kind: item.kind.to_string(),
        signature: item.signature.to_string(),
        package_path: item.path.clone(),
        package_bytes: item.package_bytes,
        blocks_verified,
        root_hash: header.root_hash.clone(),
        xex_title_id: execution.as_ref().map(|e| e.title_id.clone()),
        xex_media_id: execution.as_ref().map(|e| e.media_id.clone()),
        image_sha256,
        base_blocks,
        shift_sectors,
        image_blocks,
        extracted_at: now(),
    });
    jobrec.save(&job_dir)?;

    println!();
    println!("Job {job_id} is ready ({}).", jobrec.stage);
    println!(
        "  Next: dump-o-matic identify {job_id}    (then add --apply --accept-inferred to file it)"
    );
    Ok(Some(job_id))
}

/// Read the game's executable and return the identity it declares.
fn cross_check(
    image: &GodImage<'_>,
    header: &xcontent::XContent,
    known_base: u64,
    base_is_known: bool,
) -> Option<xex::ExecutionInfo> {
    let vd = xdvdfs::read_volume_descriptor(image).ok()?;
    let base = if base_is_known {
        known_base
    } else {
        xdvdfs::resolve_base(image, &vd, header.data_block_offset_hint()).ok()?
    };
    let record = xdvdfs::find(image, base, &vd, "default.xex").ok()??;
    let bytes = xdvdfs::read_file(image, base, &record, XEX_PREFIX).ok()?;
    xex::execution_info(&bytes)
}

/// Open a device (or directory) and catalogue it, auto-selecting one if not named.
fn resolve(spec: Option<&str>) -> Result<(Box<dyn ContentSource>, Catalogue)> {
    if let Some(spec) = spec {
        let source = open_source(spec)?;
        let catalogue = layout::detect(source.as_ref())?;
        return Ok((source, catalogue));
    }

    // No device named: offer the one that actually holds recognisable content. Removable
    // devices are tried first, which is nearly always what was just plugged in.
    let devices = dumo_devices::enumerate_devices().context("enumerating storage devices")?;
    let mut tried = Vec::new();
    for d in devices.iter().filter(|d| d.readable) {
        let Ok(source) = Fat32Source::open(Path::new(&d.path)) else {
            tried.push(format!("{} (not a FAT32 volume)", d.path));
            continue;
        };
        match layout::detect(&source) {
            Ok(c) => {
                println!("Using {} ({})", d.path, d.description());
                return Ok((Box::new(source), c));
            }
            Err(e) => tried.push(format!("{}: {e}", d.path)),
        }
    }
    let unreadable: Vec<&str> = devices
        .iter()
        .filter(|d| !d.readable)
        .map(|d| d.path.as_str())
        .collect();
    let mut message =
        String::from("no attached device holds content this tool recognises.\nChecked:");
    for t in &tried {
        message.push_str(&format!("\n  {t}"));
    }
    if !unreadable.is_empty() {
        message.push_str(&format!(
            "\nCould not read {} — add your user to the 'disk' group and log back in, \
             or mount read-only and pass the mount directory.",
            unreadable.join(", ")
        ));
    }
    bail!(message)
}

fn open_source(spec: &str) -> Result<Box<dyn ContentSource>> {
    let path = Path::new(spec);
    if path.is_dir() {
        Ok(Box::new(DirSource::open(path)?))
    } else {
        Ok(Box::new(Fat32Source::open(path)?))
    }
}

/// Pick the item to pull, requiring a choice only when there is one to make.
fn select<'a>(catalogue: &'a Catalogue, title: Option<&str>) -> Result<&'a CatalogueItem> {
    if let Some(t) = title {
        return catalogue.find(t).map_err(|e| anyhow::anyhow!(e));
    }
    let games: Vec<&CatalogueItem> = catalogue.games().collect();
    match games.len() {
        0 => bail!("this device holds no game images that can be pulled off"),
        1 => Ok(games[0]),
        _ => bail!(
            "this device holds {} games; name one with --title:\n{}",
            games.len(),
            games
                .iter()
                .map(|g| format!("  {}  {}", g.title_id, g.name))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Report progress as whole percentages, so a long extraction shows movement without
/// flooding a log.
fn report_progress(last: &mut u8, done: u64, total: u64) {
    if total == 0 {
        return;
    }
    let pct = ((done.min(total) as f64 / total as f64) * 100.0) as u8;
    if pct > *last {
        *last = pct;
        print!("\r    {pct:>3}%  {} of {}", human_size(done), human_size(total));
        std::io::stdout().flush().ok();
    }
}

fn header_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Reduce a package's display name to something safe to use as a filename.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            ':' | '?' | '*' | '"' | '<' | '>' | '|' => ' ',
            other if (other as u32) < 0x20 => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names come off removable media and go straight into filenames, so the characters a
    /// filesystem or SMB share would refuse have to go.
    #[test]
    fn sanitise_removes_path_and_wildcard_characters() {
        assert_eq!(sanitise("Frontlines:Fuel of War"), "Frontlines Fuel of War");
        assert_eq!(sanitise("CoD: World at War"), "CoD World at War");
        assert_eq!(sanitise("Some/Thing\\Else"), "Some-Thing-Else");
        assert_eq!(sanitise("a\u{1}b"), "a b");
    }

    /// A trademark sign is legal in a filename and part of the title, so it stays.
    #[test]
    fn sanitise_keeps_legal_punctuation() {
        assert_eq!(sanitise("Rainbow Six® Vegas"), "Rainbow Six® Vegas");
        assert_eq!(sanitise("Command & Conquer 3"), "Command & Conquer 3");
    }

    #[test]
    fn header_name_is_the_last_path_component() {
        assert_eq!(
            header_name("Content/0000000000000000/545407E0/00004000/ABC123"),
            "ABC123"
        );
    }

    #[test]
    fn formats_select_the_right_outputs() {
        assert!(PullFormat::Iso.wants_iso() && !PullFormat::Iso.wants_package());
        assert!(PullFormat::Package.wants_package() && !PullFormat::Package.wants_iso());
        assert!(PullFormat::Both.wants_iso() && PullFormat::Both.wants_package());
    }
}
