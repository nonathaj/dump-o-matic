//! The `tag-library` command: put the TMDB tag Plex asks for on the shows and films
//! already in permanent storage.
//!
//! Plex is told exactly which show or film a folder or file is by a tag in its name,
//! `Robot Chicken (2005) {tmdb-709}`; `identify` files everything that way. A library kept
//! before that has names Plex has to guess from, `Bones` or `300.mkv`, and a show filed
//! with the tag beside one without it would be split in two.
//!
//! A name is only tagged when TMDB leaves no doubt: one entry of that title and year.
//! A name without a year needs its running time to agree as well, since a title alone
//! is often another entry's: `Cosmos` here was Carl Sagan's, an hour an episode, where
//! TMDB's only `Cosmos` is the 2014 one at 44 minutes. So one entry of that title must
//! run as the episodes (or the film) here do. Everything else is listed with its candidates and left as it is, to be
//! settled with `--id`. Only the top-level name changes, plus a film's own file inside
//! its folder; nothing is copied, and the manifests that record those files follow them.

use anyhow::{bail, Context, Result};
use dumo_core::config::Config;
use dumo_core::job::Job;
use dumo_identify::plexname::{self, Entry};
use dumo_identify::tmdb::{MovieResult, TmdbClient, TvResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct TagArgs {
    pub apply: bool,
    pub shows: bool,
    pub movies: bool,
    /// `NAME=ID`: the TMDB entry a name is, settled by hand.
    pub ids: Vec<String>,
    /// Only these names, exactly as they are in the library.
    pub only: Vec<String>,
    /// Only the first this many names that can be tagged.
    pub batch: Option<usize>,
    /// Without `apply`, where to write the renames listed; with it, the renames to make,
    /// as an earlier run wrote them, without looking anything up again.
    pub plan: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
}

/// Films checked by runtime for a name that more than one has.
const MOVIE_CANDIDATES: usize = 5;

/// One name in the library, and what it becomes.
struct Rename {
    category: &'static str,
    root: PathBuf,
    old: String,
    new: String,
    is_dir: bool,
    id: u64,
}

/// The TMDB page of a show or film, to check a rename against.
fn tmdb_url(category: &str, id: u64) -> String {
    let kind = if category == "tv" { "tv" } else { "movie" };
    format!("https://www.themoviedb.org/{kind}/{id}")
}

enum Verdict {
    Tag { year: u32, id: u64, why: String },
    Unsure(String),
}

pub fn run(args: TagArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;
    let client = TmdbClient::from_config(&cfg.api).context("TMDB is needed to look the names up")?;
    let mut ids: HashMap<String, u64> = HashMap::new();
    for spec in &args.ids {
        let Some((name, id)) = spec.rsplit_once('=') else {
            bail!("--id {spec:?}: expected NAME=ID, e.g. \"Bones=1911\"");
        };
        let id = id
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("--id {spec:?}: the ID is TMDB's number, e.g. 1911"))?;
        ids.insert(name.trim().to_string(), id);
    }
    let categories: Vec<&'static str> = match (args.shows, args.movies) {
        (true, false) => vec!["tv"],
        (false, true) => vec!["movies"],
        _ => vec!["tv", "movies"],
    };
    let root_of = |category: &str| {
        cfg.destinations
            .iter()
            .find(|d| d.accepts(category) && !d.media.is_empty())
            .map(|d| d.root.clone())
    };

    if let (true, Some(plan)) = (args.apply, &args.plan) {
        let renames = read_plan(plan, |c| root_of(c))?;
        println!("{} rename(s) in {}:", renames.len(), plan.display());
        for r in &renames {
            // An empty library is a share that is not mounted.
            list(&r.root)?;
            println!("  {}\n    -> {}", r.old, r.new);
        }
        return apply(&cfg, &renames);
    }

    let mut renames: Vec<Rename> = Vec::new();
    let mut unsure = 0usize;
    let mut more = 0usize;
    for category in categories {
        let Some(root) = root_of(category) else {
            println!("No destination is set aside for {category}; skipping it.");
            continue;
        };
        let entries = list(&root)?;
        let tagged = entries.iter().filter(|(n, _)| plexname::tmdb_id(n).is_some()).count();
        println!();
        println!("{} ({}): {} name(s), {tagged} already tagged", root.display(), category, entries.len());
        let untagged: Vec<&(String, bool)> =
            entries.iter().filter(|(n, _)| plexname::tmdb_id(n).is_none()).collect();
        let mut judged: Vec<(&str, bool, std::result::Result<(String, u64), String>, String)> = Vec::new();
        for (i, (name, is_dir)) in untagged.iter().enumerate() {
            // Every name is looked up even with --only, so that two names becoming one
            // are still found.
            eprint!("\r  looking up {} of {} ...", i + 1, untagged.len());
            let entry = plexname::parse(name, !is_dir);
            let given = ids.get(name.as_str()).copied();
            let verdict = match category {
                "tv" => judge_show(&client, &root, name, &entry, given),
                _ => judge_movie(&client, &root, name, *is_dir, &entry, given),
            }
            .with_context(|| format!("looking up {name:?}"))?;
            judged.push(match verdict {
                Verdict::Tag { year, id, why } => (name, *is_dir, Ok((entry.tagged(year, id), id)), why),
                Verdict::Unsure(why) => (name, *is_dir, Err(why), String::new()),
            });
        }
        eprintln!();
        // Two names becoming one, or one becoming a name the library already has, are the
        // same show or film kept twice: that is for a person to merge, not a rename.
        let clashes = clashing(&entries, &judged);
        for (name, is_dir, outcome, why) in judged {
            if !args.only.is_empty() && !args.only.iter().any(|o| o == name) {
                continue;
            }
            match outcome {
                Ok((new, _)) if clashes.contains_key(&new) => {
                    unsure += 1;
                    println!("  ??   {name}: is {new}, as {} also is; merge them by hand", clashes[&new]);
                }
                Ok(_) if args.batch.map(|n| renames.len() >= n).unwrap_or(false) => more += 1,
                Ok((new, id)) => {
                    println!(
                        "  tag  {name}\n       -> {new}\n       {}   ({why})",
                        tmdb_url(category, id)
                    );
                    renames.push(Rename { category, root: root.clone(), old: name.to_string(), new, is_dir, id });
                }
                Err(why) => {
                    unsure += 1;
                    println!("  ??   {name}: {why}");
                }
            }
        }
    }
    for id_name in ids.keys() {
        if !renames.iter().any(|r| &r.old == id_name) {
            println!("note: --id names {id_name:?}, which is not an untagged name in the library");
        }
    }
    for o in &args.only {
        if !renames.iter().any(|r| &r.old == o) {
            println!("note: --only names {o:?}, which is not listed to be tagged");
        }
    }

    println!();
    println!(
        "{} name(s) to tag; {unsure} left as they are, for --id \"NAME=ID\" to settle.",
        renames.len()
    );
    if more > 0 {
        println!("{more} more can be tagged, after this batch.");
    }
    if renames.is_empty() {
        return Ok(());
    }
    if !args.apply {
        match &args.plan {
            Some(plan) => {
                write_plan(plan, &renames)?;
                println!("Nothing was renamed. These renames are in {}; make them with", plan.display());
                println!("  dump-o-matic tag-library --apply --plan {}", plan.display());
            }
            None => println!("Nothing was renamed. Re-run with --apply to rename them."),
        }
        return Ok(());
    }
    apply(&cfg, &renames)
}

/// Make the renames, in the library and in staging, and update the job manifests.
fn apply(cfg: &Config, renames: &[Rename]) -> Result<()> {
    println!();
    let mut done: Vec<&Rename> = Vec::new();
    for r in renames {
        match rename_entry(&r.root, &r.old, &r.new, r.is_dir) {
            Ok(()) => done.push(r),
            Err(e) => println!("  not renamed: {}: {e:#}", r.old),
        }
    }
    // Copies still in staging, not yet cleaned, follow the library's names.
    for r in &done {
        let staged = cfg.staging.ready_dir().join(r.category);
        if staged.join(&r.old).exists() {
            if let Err(e) = rename_entry(&staged, &r.old, &r.new, r.is_dir) {
                println!("  staged copy not renamed: {}: {e:#}", r.old);
            }
        }
    }
    let jobs = retarget_jobs(cfg, &done)?;
    println!(
        "Renamed {} of {}; {jobs} job manifest(s) updated to the new names.",
        done.len(),
        renames.len()
    );
    Ok(())
}

const PLAN_HEADER: &str = "# dump-o-matic tag-library plan: category, dir or file, TMDB ID, old name, new name";

/// Write renames one to a line, tab-separated, for `--apply --plan` to make as listed.
fn write_plan(path: &Path, renames: &[Rename]) -> Result<()> {
    let mut s = format!("{PLAN_HEADER}\n");
    for r in renames {
        if [&r.old, &r.new].iter().any(|n| n.contains(['\t', '\n'])) {
            bail!("{:?} has a tab or line break in it, which a plan cannot hold", r.old);
        }
        let kind = if r.is_dir { "dir" } else { "file" };
        s.push_str(&format!("{}\t{kind}\t{}\t{}\t{}\n", r.category, r.id, r.old, r.new));
    }
    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
}

/// Read a plan `write_plan` wrote, finding each category's library with `root_of`.
fn read_plan(path: &Path, root_of: impl Fn(&str) -> Option<PathBuf>) -> Result<Vec<Rename>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut renames = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let bad = || anyhow::anyhow!("{} line {}: not a tag-library plan line", path.display(), n + 1);
        let f: Vec<&str> = line.split('\t').collect();
        let [category, kind, id, old, new] = f.as_slice() else {
            return Err(bad());
        };
        let category: &'static str = match *category {
            "tv" => "tv",
            "movies" => "movies",
            _ => return Err(bad()),
        };
        let is_dir = match *kind {
            "dir" => true,
            "file" => false,
            _ => return Err(bad()),
        };
        let id: u64 = id.parse().map_err(|_| bad())?;
        if plexname::tmdb_id(new) != Some(id) || plexname::tmdb_id(old).is_some() {
            return Err(bad());
        }
        let root = root_of(category).with_context(|| format!("no destination is set aside for {category}"))?;
        renames.push(Rename { category, root, old: old.to_string(), new: new.to_string(), is_dir, id });
    }
    Ok(renames)
}

/// The names at the top of a library, in order, but not hidden ones or `_held` folders.
fn list(root: &Path) -> Result<Vec<(String, bool)>> {
    let mut v: Vec<(String, bool)> = std::fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().is_dir()))
        .filter(|(n, _)| !n.starts_with('.') && !n.starts_with('_'))
        .collect();
    if v.is_empty() {
        bail!("{} is empty; is the share mounted?", root.display());
    }
    v.sort();
    Ok(v)
}

/// New names that more than one name would take, or that the library already has, each
/// with what else is, or would be, that name.
fn clashing(
    entries: &[(String, bool)],
    judged: &[(&str, bool, std::result::Result<(String, u64), String>, String)],
) -> HashMap<String, String> {
    let mut by_new: HashMap<&str, Vec<&str>> = HashMap::new();
    for (old, _, outcome, _) in judged {
        if let Ok((new, _)) = outcome {
            by_new.entry(new.as_str()).or_default().push(old);
        }
    }
    by_new
        .into_iter()
        .filter_map(|(new, olds)| {
            if entries.iter().any(|(n, _)| n == new) {
                Some((new.to_string(), format!("{new:?} already here")))
            } else if olds.len() > 1 {
                Some((new.to_string(), olds.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>().join(" and ")))
            } else {
                None
            }
        })
        .collect()
}

/// Whether episodes running `here` minutes are the ones TMDB lists at `listed`: within a
/// fifth, or four minutes for short ones. Episodes of one series vary by more than a
/// film's cuts do, and series that share a title differ by far more (an hour against
/// 44 minutes, 7 against 22).
fn runs_like(here: f64, listed: f64) -> bool {
    (here - listed).abs() <= (listed * 0.2).max(4.0)
}

const VIDEO: [&str; 5] = ["mkv", "mp4", "m4v", "avi", "ts"];

/// Whether a folder in a show holds its specials or films rather than episodes:
/// `Season 00`, `Season 0`, `Specials`. Those run to any length, and sorting first, they
/// made Futurama's episodes look 89 minutes long.
fn is_specials(dir: &Path) -> bool {
    let name = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name == "specials"
        || name
            .strip_prefix("season")
            .map(|n| n.trim().trim_start_matches('0').is_empty())
            .unwrap_or(false)
}

/// How long a show's episodes run here: the median of the first few of its videos,
/// specials aside.
fn episode_minutes(dir: &Path) -> Option<f64> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() && is_specials(&p) {
                continue;
            }
            if p.is_dir() && depth < 2 {
                walk(&p, depth + 1, out);
            } else if p.extension().map(|e| VIDEO.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, 0, &mut files);
    let mut mins: Vec<f64> = files
        .iter()
        .take(5)
        .filter_map(|p| dumo_backends::ffprobe::probe(p).ok())
        .map(|f| f.duration_secs / 60.0)
        .filter(|m| *m > 0.0)
        .collect();
    median(&mut mins)
}

/// How long TMDB lists a series' episodes as running: the median over its first season.
fn listed_minutes(client: &TmdbClient, id: u64) -> Result<Option<f64>> {
    let seasons = client.tv_season_numbers(id)?;
    let Some(first) = seasons.iter().copied().filter(|n| *n > 0).min() else {
        return Ok(None);
    };
    let mut mins: Vec<f64> = client
        .season(id, first)?
        .episodes
        .iter()
        .filter_map(|e| e.runtime_mins)
        .map(f64::from)
        .collect();
    Ok(median(&mut mins))
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn judge_show(client: &TmdbClient, root: &Path, name: &str, entry: &Entry, given: Option<u64>) -> Result<Verdict> {
    if let Some(id) = given {
        let show = client.tv(id)?;
        return Ok(match entry.year.or(show.year()) {
            Some(year) => Verdict::Tag { year, id, why: format!("given: {}", describe_tv(&show)) },
            None => Verdict::Unsure(format!("TMDB lists no year for {}", describe_tv(&show))),
        });
    }
    let found = client.search_tv(&entry.title)?;
    let same: Vec<&TvResult> = found
        .iter()
        .filter(|s| plexname::same_title(&s.name, &entry.title))
        .filter(|s| entry.year.map(|y| s.year() == Some(y)).unwrap_or(true))
        .collect();
    if same.is_empty() {
        return Ok(Verdict::Unsure(format!(
            "no series of that name{}; TMDB's nearest: {}",
            entry.year.map(|y| format!(" from {y}")).unwrap_or_default(),
            listing(found.iter().take(3).map(describe_tv))
        )));
    }
    if let ([one], Some(_)) = (same.as_slice(), entry.year) {
        return Ok(tag_show(one, "the one series of that name and year"));
    }
    // No year, or several series of that title and year: the episodes' length decides.
    let Some(here) = episode_minutes(&root.join(name)) else {
        return Ok(Verdict::Unsure(format!(
            "no year, and its episodes' length could not be read: {}",
            listing(same.iter().take(4).map(|s| describe_tv(s)))
        )));
    };
    let mut fitting = Vec::new();
    let mut checked = Vec::new();
    for s in same.iter().take(MOVIE_CANDIDATES) {
        let listed = listed_minutes(client, s.id)?;
        if listed.map(|l| runs_like(here, l)).unwrap_or(false) {
            fitting.push((*s, listed.unwrap_or(0.0)));
        }
        checked.push((*s, listed));
    }
    Ok(match fitting.as_slice() {
        [(one, listed)] => tag_show(
            one,
            &format!(
                "{} of that name, and the one whose episodes run as these do, \
                 {here:.0} min here against {listed:.0} min listed",
                if same.len() == 1 { "the one series".to_string() } else { format!("{} series", same.len()) }
            ),
        ),
        [] => Verdict::Unsure(format!(
            "episodes of {here:.0} min here fit none of: {}",
            listing(checked.iter().map(|(s, l)| {
                format!("{} {} min", describe_tv(s), l.map(|l| format!("{l:.0}")).unwrap_or("?".into()))
            }))
        )),
        many => Verdict::Unsure(format!(
            "episodes of {here:.0} min here fit {} of them: {}",
            many.len(),
            listing(many.iter().map(|(s, _)| describe_tv(s)))
        )),
    })
}

fn tag_show(show: &TvResult, why: &str) -> Verdict {
    match show.year() {
        Some(year) => Verdict::Tag { year, id: show.id, why: format!("{why}: {}", describe_tv(show)) },
        None => Verdict::Unsure(format!("TMDB lists no year for {}", describe_tv(show))),
    }
}

fn judge_movie(
    client: &TmdbClient,
    root: &Path,
    name: &str,
    is_dir: bool,
    entry: &Entry,
    given: Option<u64>,
) -> Result<Verdict> {
    if let Some(id) = given {
        let film = client.movie(id)?;
        return Ok(match entry.year.or(film.year()) {
            Some(year) => Verdict::Tag { year, id, why: format!("given: {}", describe_movie(&film)) },
            None => Verdict::Unsure(format!("TMDB lists no year for {}", describe_movie(&film))),
        });
    }
    let found = client.search_movie(&entry.title)?;
    let same: Vec<&MovieResult> = found
        .iter()
        .filter(|m| plexname::same_title(&m.title, &entry.title))
        .filter(|m| entry.year.map(|y| m.year() == Some(y)).unwrap_or(true))
        .collect();
    if same.is_empty() {
        return Ok(Verdict::Unsure(format!(
            "no film of that name{}; TMDB's nearest: {}",
            entry.year.map(|y| format!(" from {y}")).unwrap_or_default(),
            listing(found.iter().take(3).map(describe_movie))
        )));
    }
    if let ([one], Some(_)) = (same.as_slice(), entry.year) {
        return Ok(tag_film(one, "the one film of that name and year"));
    }
    // A part of a film runs for only some of it, so its length says nothing.
    if !entry.rest.is_empty() {
        return Ok(Verdict::Unsure(format!(
            "{} film(s) of that name, and a part of a film cannot be told by its length: {}",
            same.len(),
            listing(same.iter().take(4).map(|m| describe_movie(m)))
        )));
    }
    let Some(mins) = film_minutes(root, name, is_dir) else {
        return Ok(Verdict::Unsure(format!(
            "no year, and its length could not be read: {}",
            listing(same.iter().take(4).map(|m| describe_movie(m)))
        )));
    };
    let mut fitting = Vec::new();
    let mut checked = Vec::new();
    for m in same.iter().take(MOVIE_CANDIDATES) {
        let film = client.movie(m.id)?;
        if film.runtime_mins.map(|r| dumo_identify::movie::fits(mins, r)).unwrap_or(false) {
            fitting.push(film.clone());
        }
        checked.push(film);
    }
    Ok(match fitting.as_slice() {
        [one] => tag_film(
            one,
            &format!(
                "{} of that name, and the one whose runtime fits: {} min listed, {mins:.0} min here",
                if same.len() == 1 { "the one film".to_string() } else { format!("{} films", same.len()) },
                one.runtime_mins.unwrap_or(0)
            ),
        ),
        [] => Verdict::Unsure(format!(
            "{mins:.0} min here fits none of: {}",
            listing(checked.iter().map(|m| {
                format!("{} {} min", describe_movie(m), m.runtime_mins.map(|r| r.to_string()).unwrap_or("?".into()))
            }))
        )),
        many => Verdict::Unsure(format!(
            "{mins:.0} min here fits {} of them: {}",
            many.len(),
            listing(many.iter().map(describe_movie))
        )),
    })
}

fn tag_film(film: &MovieResult, why: &str) -> Verdict {
    match film.year() {
        Some(year) => Verdict::Tag { year, id: film.id, why: format!("{why}: {}", describe_movie(film)) },
        None => Verdict::Unsure(format!("TMDB lists no year for {}", describe_movie(film))),
    }
}

/// A film's running time: the file's, or in a film's folder the longest video in it.
fn film_minutes(root: &Path, name: &str, is_dir: bool) -> Option<f64> {
    let files: Vec<PathBuf> = if is_dir {
        std::fs::read_dir(root.join(name))
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect()
    } else {
        vec![root.join(name)]
    };
    files
        .iter()
        .filter_map(|p| dumo_backends::ffprobe::probe(p).ok())
        .map(|f| f.duration_secs / 60.0)
        .fold(None, |best: Option<f64>, m| Some(best.map_or(m, |b| b.max(m))))
}

fn describe_tv(s: &TvResult) -> String {
    format!("{}{} [tmdb {}]", s.name, s.year().map(|y| format!(" ({y})")).unwrap_or_default(), s.id)
}

fn describe_movie(m: &MovieResult) -> String {
    format!("{}{} [tmdb {}]", m.title, m.year().map(|y| format!(" ({y})")).unwrap_or_default(), m.id)
}

fn listing(items: impl Iterator<Item = String>) -> String {
    let v: Vec<String> = items.collect();
    if v.is_empty() {
        "nothing".into()
    } else {
        v.join("; ")
    }
}

/// Rename a show folder, a film file with the files beside it that share its name
/// (`Title.en.srt`), or a film's folder with the film's own file inside it. Refused when
/// the new name is taken: nothing is overwritten.
fn rename_entry(root: &Path, old: &str, new: &str, is_dir: bool) -> Result<()> {
    let to = root.join(new);
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    std::fs::rename(root.join(old), &to).with_context(|| format!("renaming to {}", to.display()))?;
    if is_dir {
        // The film's own file is named for its folder; a show's episodes are not.
        for e in std::fs::read_dir(&to)?.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(tail) = n.strip_prefix(old).filter(|t| t.starts_with('.')) {
                let dest = to.join(format!("{new}{tail}"));
                if !dest.exists() {
                    std::fs::rename(e.path(), dest)?;
                }
            }
        }
    } else {
        let (old_stem, new_stem) = (stem(old), stem(new));
        for e in std::fs::read_dir(root)?.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(tail) = n.strip_prefix(old_stem).filter(|t| t.starts_with('.')) {
                let dest = root.join(format!("{new_stem}{tail}"));
                if !dest.exists() && e.path().is_file() {
                    std::fs::rename(e.path(), dest)?;
                }
            }
        }
    }
    Ok(())
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

/// A staged path under its new name, if a rename touches it.
fn renamed_path(path: &str, r: &Rename) -> Option<String> {
    let base = format!("ready/{}/", r.category);
    let below = path.strip_prefix(&base)?;
    if r.is_dir {
        let inside = below.strip_prefix(&r.old)?.strip_prefix('/')?;
        // A film's own file inside its folder is renamed with it.
        let inside = match inside.strip_prefix(&r.old).filter(|t| t.starts_with('.') && r.category == "movies") {
            Some(tail) => format!("{}{tail}", r.new),
            None => inside.to_string(),
        };
        Some(format!("{base}{}/{inside}", r.new))
    } else {
        let tail = below.strip_prefix(stem(&r.old))?.to_string();
        tail.starts_with('.').then(|| format!("{base}{}{tail}", stem(&r.new)))
    }
}

/// Point every job's record of a renamed file at its new name. Returns how many changed.
fn retarget_jobs(cfg: &Config, done: &[&Rename]) -> Result<usize> {
    let jobs_dir = cfg.staging.jobs_dir();
    let mut changed = 0;
    for dir in std::fs::read_dir(&jobs_dir)?.filter_map(|e| e.ok()).map(|e| e.path()) {
        let Ok(mut job) = Job::load(&dir) else {
            continue;
        };
        let Some(id) = job.identification.as_mut() else {
            continue;
        };
        let mut touched = false;
        let mut apply = |p: &mut String| {
            if let Some(n) = done.iter().find_map(|r| renamed_path(p, r)) {
                *p = n;
                touched = true;
            }
        };
        for f in &mut id.files {
            apply(&mut f.path);
        }
        for s in &mut id.superseded {
            apply(s);
        }
        for t in &mut id.trimmed {
            apply(&mut t.filed);
        }
        if touched {
            job.save(&dir).with_context(|| format!("saving {}", dir.display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename(category: &'static str, old: &str, new: &str, is_dir: bool) -> Rename {
        let id = plexname::tmdb_id(new).unwrap_or(0);
        Rename { category, root: PathBuf::new(), old: old.into(), new: new.into(), is_dir, id }
    }

    /// A batch reviewed in a dry run is the batch applied, name for name.
    #[test]
    fn a_plan_reads_back_as_written() {
        let path = std::env::temp_dir().join(format!("dumo-plan-{}.tsv", std::process::id()));
        let renames = vec![
            rename("tv", "Robot Chicken (2005)", "Robot Chicken (2005) {tmdb-709}", true),
            rename("movies", "300.mkv", "300 (2007) {tmdb-1271}.mkv", false),
        ];
        write_plan(&path, &renames).unwrap();
        let back = read_plan(&path, |c| Some(PathBuf::from(format!("/lib/{c}")))).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(
            (back[0].category, back[0].old.as_str(), back[0].new.as_str(), back[0].is_dir, back[0].id),
            ("tv", "Robot Chicken (2005)", "Robot Chicken (2005) {tmdb-709}", true, 709)
        );
        assert_eq!(back[1].root, PathBuf::from("/lib/movies"));
        assert!(!back[1].is_dir);

        // A new name whose tag disagrees with the ID is not a plan this wrote.
        std::fs::write(&path, "tv\tdir\t1\tBones\tBones (2005) {tmdb-1911}\n").unwrap();
        assert!(read_plan(&path, |_| Some(PathBuf::new())).is_err());
        let _ = std::fs::remove_file(&path);
        assert_eq!(tmdb_url("tv", 709), "https://www.themoviedb.org/tv/709");
        assert_eq!(tmdb_url("movies", 1271), "https://www.themoviedb.org/movie/1271");
    }

    /// Series that share a title, told apart by how long their episodes run.
    #[test]
    fn episodes_run_like_their_series_and_not_its_namesakes() {
        assert!(runs_like(58.0, 60.0), "Sagan's Cosmos against its listing");
        assert!(!runs_like(58.0, 44.0), "Sagan's Cosmos against the 2014 one");
        assert!(!runs_like(7.0, 22.0), "a Tom and Jerry short against a half-hour series");
        assert!(runs_like(11.0, 12.0));
    }

    #[test]
    fn specials_folders_are_not_sampled_for_episode_length() {
        for n in ["Season 00", "Season 0", "season 00", "Specials"] {
            assert!(is_specials(Path::new(n)), "{n}");
        }
        for n in ["Season 01", "Season 1", "Season 10", "Golden Collection Volume 1"] {
            assert!(!is_specials(Path::new(n)), "{n}");
        }
    }

    #[test]
    fn two_names_for_one_show_are_a_clash_not_a_rename() {
        let entries = vec![
            ("Friends".to_string(), true),
            ("Friends (1994)".to_string(), true),
            ("Carnivale".to_string(), true),
            ("Carnivale (2003) {tmdb-185}".to_string(), true),
            ("Bones".to_string(), true),
        ];
        let judged = vec![
            ("Friends", true, Ok(("Friends (1994) {tmdb-1668}".to_string(), 1668)), String::new()),
            ("Friends (1994)", true, Ok(("Friends (1994) {tmdb-1668}".to_string(), 1668)), String::new()),
            ("Carnivale", true, Ok(("Carnivale (2003) {tmdb-185}".to_string(), 185)), String::new()),
            ("Bones", true, Ok(("Bones (2005) {tmdb-1911}".to_string(), 1911)), String::new()),
        ];
        let c = clashing(&entries, &judged);
        assert!(c.contains_key("Friends (1994) {tmdb-1668}"));
        assert!(c.contains_key("Carnivale (2003) {tmdb-185}"));
        assert!(!c.contains_key("Bones (2005) {tmdb-1911}"));
    }

    #[test]
    fn a_show_folder_rename_moves_every_episode_in_it() {
        let r = rename("tv", "Bones", "Bones (2005) {tmdb-1911}", true);
        assert_eq!(
            renamed_path("ready/tv/Bones/Season 01/Bones S01E01 - Pilot.mkv", &r).as_deref(),
            Some("ready/tv/Bones (2005) {tmdb-1911}/Season 01/Bones S01E01 - Pilot.mkv")
        );
        assert_eq!(renamed_path("ready/tv/Bones Brigade/Season 01/x.mkv", &r), None);
        assert_eq!(renamed_path("ready/movies/Bones/x.mkv", &r), None);
    }

    #[test]
    fn a_film_file_rename_takes_its_subtitles_along() {
        let r = rename("movies", "300.mkv", "300 (2007) {tmdb-1271}.mkv", false);
        assert_eq!(renamed_path("ready/movies/300.mkv", &r).as_deref(), Some("ready/movies/300 (2007) {tmdb-1271}.mkv"));
        assert_eq!(
            renamed_path("ready/movies/300.en.srt", &r).as_deref(),
            Some("ready/movies/300 (2007) {tmdb-1271}.en.srt")
        );
        assert_eq!(renamed_path("ready/movies/3000.mkv", &r), None);
    }

    #[test]
    fn a_film_folder_rename_renames_the_film_but_not_its_extras() {
        let r = rename("movies", "KPop Demon Hunters (2025)", "KPop Demon Hunters (2025) {tmdb-803796}", true);
        assert_eq!(
            renamed_path("ready/movies/KPop Demon Hunters (2025)/KPop Demon Hunters (2025).mkv", &r).as_deref(),
            Some("ready/movies/KPop Demon Hunters (2025) {tmdb-803796}/KPop Demon Hunters (2025) {tmdb-803796}.mkv")
        );
        assert_eq!(
            renamed_path("ready/movies/KPop Demon Hunters (2025)/Featurettes/Making Of.mkv", &r).as_deref(),
            Some("ready/movies/KPop Demon Hunters (2025) {tmdb-803796}/Featurettes/Making Of.mkv")
        );
    }

    #[test]
    fn renaming_on_disk_moves_the_film_and_its_sidecar_and_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("dumo-tag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Film (2020)")).unwrap();
        std::fs::write(dir.join("Film (2020)/Film (2020).mkv"), b"x").unwrap();
        std::fs::write(dir.join("300.mkv"), b"x").unwrap();
        std::fs::write(dir.join("300.en.srt"), b"x").unwrap();
        std::fs::write(dir.join("3000.mkv"), b"x").unwrap();

        rename_entry(&dir, "Film (2020)", "Film (2020) {tmdb-1}", true).unwrap();
        assert!(dir.join("Film (2020) {tmdb-1}/Film (2020) {tmdb-1}.mkv").is_file());
        rename_entry(&dir, "300.mkv", "300 (2007) {tmdb-2}.mkv", false).unwrap();
        assert!(dir.join("300 (2007) {tmdb-2}.mkv").is_file());
        assert!(dir.join("300 (2007) {tmdb-2}.en.srt").is_file());
        assert!(dir.join("3000.mkv").is_file(), "another film sharing the first letters is left alone");

        std::fs::write(dir.join("3000 (2001) {tmdb-3}.mkv"), b"x").unwrap();
        assert!(rename_entry(&dir, "3000.mkv", "3000 (2001) {tmdb-3}.mkv", false).is_err());
        assert!(dir.join("3000.mkv").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
