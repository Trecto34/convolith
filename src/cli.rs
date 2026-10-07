//! Command-line front end. Thin: every command calls the library modules.

use crate::collect;
use crate::config::Config;
use crate::dataset::{self, Layout};
use crate::discover::{self, DiscoverOptions, DiscoverStats};
use crate::importer::{ImportOptions, Importer};
use crate::parser::Registry;
use crate::scratch::Scratch;
use crate::secrets::SecretPolicy;
use crate::source::{Probe, Source};
use crate::{report, search, validate};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "convolith",
    version,
    about = "Canonicalize fragmented AI conversation history"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List the sources under PATH (or the well-known local stores) and the parser each would use
    Discover {
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
        #[arg(long)]
        local: bool,
    },
    /// Show how every parser sees one file or directory
    Inspect {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Import PATH into a canonical dataset
    Import {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
        #[arg(long, short)]
        output: PathBuf,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_parser = ["redact", "preserve"])]
        secret_policy: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Disable the progress bar
        #[arg(long)]
        no_progress: bool,
        /// Force display of the progress bar
        #[arg(long, conflicts_with = "no_progress")]
        progress: bool,
    },
    /// Find known AI stores on this machine / WSL distros / SSH hosts and import them
    Collect {
        /// This machine (native Windows or Linux)
        #[arg(long)]
        local: bool,
        /// Every WSL distro (via wsl.exe)
        #[arg(long)]
        wsl: bool,
        /// Remote host via the system ssh (repeatable)
        #[arg(long, value_name = "USER@HOST")]
        ssh: Vec<String>,
        /// --local + --wsl + every concrete Host in ~/.ssh/config
        #[arg(long)]
        all_machines: bool,
        /// Only these apps (prefix match, e.g. claude,codex)
        #[arg(long, value_delimiter = ',')]
        apps: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Also scan (depth 4, under home only) for known store directories
        #[arg(long)]
        deep: bool,
        #[arg(long)]
        resume: bool,
        #[arg(long, short, default_value = "canonical-ai-history")]
        output: PathBuf,
        #[arg(long, value_parser = ["redact", "preserve"])]
        secret_policy: Option<String>,
    },
    /// Export every event of ARCHIVE as one chronological JSONL stream (convolith.all/v1)
    All {
        archive: PathBuf,
        /// Write to this file (atomically)
        #[arg(
            long,
            short,
            conflicts_with = "stdout",
            required_unless_present = "stdout"
        )]
        output: Option<PathBuf>,
        /// Write JSONL to stdout (diagnostics go to stderr)
        #[arg(long)]
        stdout: bool,
    },
    /// Check a dataset; exit 1 on any FAIL
    Validate { dir: PathBuf },
    /// Pack a complete canonical dataset into one portable file
    Pack {
        archive: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Unpack a portable dataset file
    Unpack {
        pack: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Print dataset counts
    Stats { dir: PathBuf },
    /// Full-text search
    Search {
        dir: PathBuf,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Regenerate the reports
    Report { dir: PathBuf },
    /// Rebuild the search index
    RebuildIndex { dir: PathBuf },
    /// Every observation of an event
    Provenance { dir: PathBuf, event_id: String },
    /// Print the canonical record of an event
    InspectEvent { dir: PathBuf, event_id: String },
    /// List built-in parsers
    Parsers {
        #[command(subcommand)]
        cmd: Option<ParsersCmd>,
    },
}

#[derive(Subcommand)]
pub enum ParsersCmd {
    /// Show every parser's detection verdict for SOURCE
    Inspect { source: PathBuf },
}

pub fn main() -> i32 {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("convolith: {e:#}");
            2
        }
    }
}

pub fn run(cli: Cli) -> Result<i32> {
    let registry = crate::parsers::registry();
    match cli.cmd {
        Cmd::Discover { paths, local } => discover_cmd(&registry, paths, local),
        Cmd::Inspect { paths } => {
            let mut last_code = 0;
            for (i, path) in paths.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                let code = inspect(&registry, path)?;
                if code != 0 {
                    last_code = code;
                }
            }
            Ok(last_code)
        }
        Cmd::Import {
            paths,
            output,
            resume,
            dry_run,
            secret_policy,
            config,
            no_progress,
            progress,
        } => {
            let cfg = match &config {
                Some(p) => Config::load(p)?,
                None => Config::default(),
            };
            let policy = secret_policy
                .as_deref()
                .map(|s| SecretPolicy::parse(s).context("bad --secret-policy"))
                .transpose()?;
            let opts = ImportOptions {
                output,
                resume,
                dry_run,
                secrets: cfg.secret_policy(policy)?,
                args: std::env::args().skip(1).collect(),
                ..Default::default()
            }
            .from_limits(&cfg.limits);
            let mode = crate::progress::ProgressMode::from_flags(progress, no_progress);
            import(&registry, cfg, opts, &paths, mode)
        }
        Cmd::Collect {
            local,
            wsl,
            ssh,
            all_machines,
            apps,
            dry_run,
            deep,
            resume,
            output,
            secret_policy,
        } => {
            let policy = secret_policy
                .as_deref()
                .map(|s| SecretPolicy::parse(s).context("bad --secret-policy"))
                .transpose()?;
            let req = collect::Request {
                local,
                wsl,
                ssh,
                all_machines,
                apps,
                dry_run,
                deep,
                resume,
                output: output.clone(),
                secrets: Config::default().secret_policy(policy)?,
                ssh_config: collect::read_ssh_config(),
            };
            let os = if cfg!(windows) {
                discover::LocalOs::Windows
            } else {
                discover::LocalOs::Linux
            };
            let host = collect::Host {
                os,
                env: &|k| std::env::var(k).ok(),
                is_dir: &|p| p.is_dir(),
            };
            let summary = collect::run(&collect::SysRunner, &host, &req)?;
            if !dry_run {
                println!("Deduplicating...");
                if output.join("provenance").join("provenance.sqlite").exists() {
                    let pb = crate::progress::create_import_progress(
                        crate::progress::ProgressMode::Auto,
                        None,
                    );
                    let v = finalize_with_progress(&output, Some(&pb))?;
                    crate::progress::finish_progress(&pb, "done");
                    if !v.passed() {
                        print!("{}", v.summary());
                    }
                }
            }
            println!("Done.");
            for m in &summary.machines {
                if let Some(e) = &m.error {
                    eprintln!("convolith: {}: {e}", m.label);
                } else if !dry_run && m.collected_roots + m.skipped_roots > 0 {
                    eprintln!(
                        "{}: {} store(s) imported, {} unchanged",
                        m.label, m.collected_roots, m.skipped_roots
                    );
                }
            }
            Ok(summary.exit_code())
        }
        Cmd::All {
            archive,
            output,
            stdout,
        } => {
            let stats = match (output, stdout) {
                (Some(o), false) => crate::all_export::export_to_file(&archive, &o)?,
                _ => crate::all_export::export_to_stdout(&archive)?,
            };
            eprintln!("{}", stats.summary());
            Ok(0)
        }
        Cmd::Validate { dir } => {
            let r = validate::validate(&dir)?;
            print!("{}", r.summary());
            Ok(if r.passed() { 0 } else { 1 })
        }
        Cmd::Pack {
            archive,
            output,
            force,
        } => {
            crate::pack::pack(&archive, &output, force)?;
            Ok(0)
        }
        Cmd::Unpack {
            pack,
            output,
            force,
        } => {
            crate::pack::unpack(&pack, &output, force)?;
            Ok(0)
        }
        Cmd::Stats { dir } => {
            println!("{:#?}", report::stats(&dir)?);
            Ok(0)
        }
        Cmd::Search { dir, query, limit } => {
            let hits = search::search(&dir, &query, limit)?;
            for h in &hits {
                println!("{}", h.line());
            }
            eprintln!("{} hit(s)", hits.len());
            Ok(0)
        }
        Cmd::Report { dir } => {
            for p in report::generate(&dir)?.written {
                println!("{}", p.display());
            }
            Ok(0)
        }
        Cmd::RebuildIndex { dir } => {
            let s = search::rebuild_index(&dir)?;
            println!(
                "indexed {} event(s) from {} shard(s)",
                s.events_indexed, s.shards
            );
            Ok(0)
        }
        Cmd::Provenance { dir, event_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report::provenance(&dir, &event_id)?)?
            );
            Ok(0)
        }
        Cmd::InspectEvent { dir, event_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report::inspect_event(&dir, &event_id)?)?
            );
            Ok(0)
        }
        Cmd::Parsers { cmd: None } => {
            for p in registry.all() {
                println!(
                    "{:<14} {}/{}  {}",
                    p.id(),
                    p.provider(),
                    p.application(),
                    p.description()
                );
            }
            Ok(0)
        }
        Cmd::Parsers {
            cmd: Some(ParsersCmd::Inspect { source }),
        } => inspect(&registry, &source),
    }
}

fn discover_cmd(registry: &Registry, paths: Vec<PathBuf>, local: bool) -> Result<i32> {
    let (roots, opts) = match (!paths.is_empty(), local) {
        (true, _) => (paths, DiscoverOptions::default()),
        (false, true) => (
            discover::local_roots()
                .into_iter()
                .map(|r| r.path)
                .collect(),
            DiscoverOptions {
                skip_dirs: discover::local_skip_dirs(),
                known_stores_only: true,
                ..Default::default()
            },
        ),
        (false, false) => bail!("give a PATH or --local"),
    };
    if roots.is_empty() {
        eprintln!("no local stores found");
    }
    let tmp = std::env::temp_dir();
    let scratch = Scratch::create(&tmp, "convolith-discover")?;
    let mut stats = DiscoverStats::default();
    let mut found = 0u64;
    for root in &roots {
        let mut on_source = |s: &Source, probe: &Probe| -> Result<()> {
            match registry.best(probe) {
                Some(d) => {
                    found += 1;
                    println!(
                        "{:<12} {:<8} {} ({} bytes)",
                        d.parser,
                        d.confidence.as_str(),
                        s.display_path,
                        s.size
                    );
                }
                None => println!(
                    "{:<12} {:<8} {} ({} bytes)",
                    "-", "-", s.display_path, s.size
                ),
            }
            Ok(())
        };
        if let Err(e) = discover::walk_root(
            root,
            &opts,
            &Config::default(),
            scratch.path(),
            &mut on_source,
            &mut stats,
        ) {
            eprintln!("convolith: {}: {e:#}", root.display());
        }
    }
    for u in &stats.unresolved_sources {
        println!(
            "{:<12} {:<8} {} ({} bytes) {}",
            "unresolved",
            "-",
            u["display_path"].as_str().unwrap_or(""),
            u["size"],
            u["note"].as_str().unwrap_or("")
        );
    }
    for e in &stats.errors {
        eprintln!("note: {e}");
    }
    eprintln!("{} file(s) seen, {found} recognised", stats.files_seen);
    Ok(0)
}

fn inspect(registry: &Registry, path: &Path) -> Result<i32> {
    if path.is_dir() {
        return discover_cmd(registry, vec![path.to_path_buf()], false);
    }
    let probe = discover::probe_file(path)?;
    println!("{} ({} bytes)", probe.full_path, probe.size);
    for p in registry.all() {
        let d = p.detect(&probe);
        println!(
            "{:<14} {:<8} {}",
            d.parser,
            d.confidence.as_str(),
            d.reasons.join("; ")
        );
    }
    match registry.best(&probe) {
        Some(d) => println!("=> {} ({})", d.parser, d.confidence.as_str()),
        None => println!("=> unsupported"),
    }
    Ok(0)
}

fn import(
    registry_unused: &Registry,
    cfg: Config,
    opts: ImportOptions,
    inputs: &[PathBuf],
    progress_mode: crate::progress::ProgressMode,
) -> Result<i32> {
    let _ = registry_unused;
    let (dry_run, output) = (opts.dry_run, opts.output.clone());

    let total = if inputs.iter().all(|p| p.is_file()) {
        Some(inputs.len() as u64)
    } else {
        None
    };

    let pb = crate::progress::create_import_progress(progress_mode, total);
    let mut importer = Importer::new(opts, cfg, crate::parsers::registry())?;
    importer.set_progress(pb.clone());

    let stats = importer.import(inputs)?;
    if dry_run {
        crate::progress::finish_progress(&pb, "done (dry run)");
        println!(
            "sources: {} candidate, {} supported, {} unsupported, {} failed; events: {} new, {} duplicate; parse errors: {}",
            stats.candidate_sources,
            stats.supported_sources,
            stats.unsupported_sources,
            stats.sources_failed,
            stats.events_new,
            stats.events_duplicate,
            stats.parse_errors,
        );
        return Ok(0);
    }

    let v = finalize_with_progress(&output, Some(&pb))?;
    crate::progress::finish_progress(&pb, "done");

    println!(
        "sources: {} candidate, {} supported, {} unsupported, {} failed; events: {} new, {} duplicate; parse errors: {}",
        stats.candidate_sources,
        stats.supported_sources,
        stats.unsupported_sources,
        stats.sources_failed,
        stats.events_new,
        stats.events_duplicate,
        stats.parse_errors,
    );
    print!("{}", v.summary());
    Ok(if v.passed() { 0 } else { 1 })
}

/// Everything after the importer: reports, README, schema copy, search index,
/// checksums, validation.
pub fn finalize(ds: &Path) -> Result<validate::ValidationReport> {
    finalize_with_progress(ds, None)
}

pub fn finalize_with_progress(
    ds: &Path,
    pb: Option<&indicatif::ProgressBar>,
) -> Result<validate::ValidationReport> {
    if let Some(pb) = pb {
        pb.set_message("generating reports...");
    }
    report::generate(ds)?;
    write_readme(ds)?;
    copy_schema(ds)?;

    if let Some(pb) = pb {
        pb.set_message("rebuilding search index...");
    }
    search::rebuild_index(ds)?;

    if let Some(pb) = pb {
        pb.set_message("writing checksums...");
    }
    dataset::write_checksums(ds, &derived_files(ds))?;

    if let Some(pb) = pb {
        pb.set_message("validating dataset...");
    }
    validate::validate(ds)
}

/// Files that may change after the checksums are written.
fn derived_files(ds: &Path) -> Vec<PathBuf> {
    ["indexes", "staging"]
        .iter()
        .map(|d| ds.join(d))
        .filter(|d| d.is_dir())
        .flat_map(|d| walkdir::WalkDir::new(d).into_iter().filter_map(|e| e.ok()))
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

fn write_readme(ds: &Path) -> Result<()> {
    let s = report::stats(ds)?;
    let body = format!(
        "# convolith canonical dataset\n\n\
Produced by convolith {}. {} event(s), {} conversation(s), {} source(s).\n\n\
- `data/` canonical event shards (`*.jsonl.zst`) and aggregates\n\
- `indexes/` derived full-text index (`convolith search`, `convolith rebuild-index`)\n\
- `provenance.sqlite` where every event was observed\n\
- `reports/` import, coverage, duplicate, conflict, error and privacy reports\n\
- `schema/` the public JSON schemas, when shipped with the tool\n\
- `checksums.sha256` hashes of every produced file (`convolith validate`)\n",
        env!("CARGO_PKG_VERSION"),
        s.events_in_shards,
        s.conversations,
        s.sources,
    );
    dataset::write_atomic(&Layout { root: ds.into() }.readme(), body.as_bytes())
}

/// Copy `spec/*.schema.json` into `schema/` when a spec directory is found
/// (current directory, else the source tree this binary was built from).
fn copy_schema(ds: &Path) -> Result<()> {
    let spec = [
        PathBuf::from("spec"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("spec"),
    ]
    .into_iter()
    .find(|p| p.is_dir());
    let Some(spec) = spec else { return Ok(()) };
    let dst = Layout { root: ds.into() }.schema_dir();
    std::fs::create_dir_all(&dst)?;
    for e in std::fs::read_dir(spec)?.filter_map(|e| e.ok()) {
        if e.file_name().to_string_lossy().ends_with(".schema.json") {
            dataset::write_atomic(&dst.join(e.file_name()), &std::fs::read(e.path())?)?;
        }
    }
    Ok(())
}
