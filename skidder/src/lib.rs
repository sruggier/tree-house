use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{self, AtomicUsize};
use std::time::Duration;
use std::{fs, io, thread};

use anyhow::{Context, Result, bail, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use ruzstd::frame::ReadFrameHeaderError;
use ruzstd::frame_decoder::FrameDecoderError;
use ruzstd::{BlockDecodingStrategy, FrameDecoder};
use serde::{Deserialize, Serialize};

#[cfg(not(windows))]
const LIB_EXTENSION: &str = "so";
#[cfg(windows)]
const LIB_EXTENSION: &str = "dll";

mod build;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub repos: Vec<Repo>,
    pub index: PathBuf,
    pub verbose: bool,
}

impl Config {
    pub fn compiled_parser_path(&self, grammar: &str) -> Option<(String, PathBuf)> {
        let (repo, metadata) = self.repos.iter().find_map(|repo| {
            let metadata = repo.read_metadata(self, grammar).ok()?;
            Some((repo, metadata))
        })?;

        let grammar = match metadata {
            Metadata::ReuseParser { name, .. } => name,
            Metadata::ParserDefinition { .. } => grammar.to_string(),
        };

        let parser = repo
            .dir(self)
            .join(&grammar)
            .join(&grammar)
            .with_extension(LIB_EXTENSION);
        parser.exists().then_some((grammar, parser))
    }

    pub fn grammar_dir(&self, grammar: &str) -> Option<PathBuf> {
        self.repos.iter().find_map(|repo| {
            repo.has_grammar(self, grammar)
                .then(|| repo.dir(self).join(grammar))
        })
    }

    fn git(&self, args: &[&str], dir: &Path) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        if self.verbose {
            println!("{}: git {}", dir.display(), args.join(" "))
        }
        let status = if self.verbose {
            cmd.status().context("failed to invoke git")?
        } else {
            let res = cmd.output().context("failed to invoke git")?;
            if !res.status.success() {
                let _ = io::stdout().write_all(&res.stdout);
                let _ = io::stderr().write_all(&res.stderr);
            }
            res.status
        };
        if !status.success() {
            bail!("git returned non-zero exit-code: {status}");
        }
        Ok(())
    }

    // TODO: remove?
    #[allow(dead_code)]
    fn git_exit_with(&self, args: &[&str], dir: &Path, exitcode: i32) -> Result<bool> {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        if self.verbose {
            println!("{}: git {}", dir.display(), args.join(" "))
        }
        if !self.verbose {
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
        }
        let status = cmd.status().context("failed to invoke git")?;
        if status.code() == Some(exitcode) {
            return Ok(true);
        }
        if !status.success() {
            bail!("git returned unexpected exit-code: {status}");
        }
        Ok(false)
    }

    fn git_output(&self, args: &[&str], dir: &Path) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        if self.verbose {
            println!("{}: git {}", dir.display(), args.join(" "))
        }
        let res = cmd.output().context("failed to invoke git")?;
        if !res.status.success() {
            let _ = io::stdout().write_all(&res.stdout);
            let _ = io::stderr().write_all(&res.stderr);
            bail!("git returned non-zero exit-code: {}", res.status);
        }
        String::from_utf8(res.stdout).context("git returned invalid utf8")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Repo {
    Git {
        name: String,
        remote: String,
        branch: String,
    },
    Local {
        path: PathBuf,
    },
}

impl Repo {
    pub fn dir(&self, config: &Config) -> PathBuf {
        match self {
            Repo::Git { name, .. } => config.index.join(name),
            Repo::Local { path } => path.clone(),
        }
    }

    pub fn has_grammar(&self, config: &Config, grammar: &str) -> bool {
        self.dir(config)
            .join(grammar)
            .join("metadata.json")
            .exists()
    }

    pub fn read_metadata(&self, config: &Config, grammar: &str) -> Result<Metadata> {
        let path = self.dir(config).join(grammar).join("metadata.json");
        Metadata::read(&path).with_context(|| format!("failed to read metadata for {grammar}"))
    }

    pub fn list_grammars(&self, config: &Config) -> Result<Vec<PathBuf>> {
        let dir = self.dir(config);
        if !dir.exists() {
            return Ok(vec![]);
        }
        fs::read_dir(&dir)
            .with_context(|| format!("failed to access repository {}", dir.display()))?
            .map(|dent| {
                let dent =
                    dent.with_context(|| format!("failed to access repository {}", dir.display()))?;
                if !dent.file_type()?.is_dir() || dent.file_name().to_str().is_none() {
                    return Ok(None);
                }
                let path = dent.path();
                let metadata_file = path.join("metadata.json");
                if !metadata_file.exists() {
                    return Ok(None);
                }
                let metadata = Metadata::read(&metadata_file).with_context(|| {
                    format!("failed to read metadata file {}", metadata_file.display())
                })?;
                Ok(metadata.parser_definition().map(|_| dent.path()))
            })
            .filter_map(|res| res.transpose())
            .collect()
    }

    pub fn fetch(&self, config: &Config, update: bool) -> Result<()> {
        let Repo::Git { remote, branch, .. } = self else {
            return Ok(());
        };
        let dir = self.dir(config);
        if dir.join(".git").exists() {
            let current_branch = config.git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &dir)?;
            let switch_branch = current_branch != *branch;
            if !update && !switch_branch {
                return Ok(());
            }
            if switch_branch {
                config.git(&["reset", "--hard"], &dir)?;
                // Cloning with `--single-branch` sets the `remote.origin.fetch`
                // spec to only fetch the desired branch. Switch this branch to
                // the new desired branch.
                config.git(
                    &[
                        "config",
                        "remote.origin.fetch",
                        &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                    ],
                    &dir,
                )?;
            }
            config.git(&["fetch", "origin", branch], &dir)?;
            if switch_branch {
                // Note that `git switch <branch>` exists but is marked as experimental
                // at time of writing. `git checkout <existing branch>` is the tried and
                // true alternative.
                config.git(&["checkout", branch], &dir)?;
            }
            config.git(&["reset", "--hard", &format!("origin/{branch}")], &dir)?;
            return Ok(());
        }
        let _ = fs::create_dir_all(&dir);
        ensure!(dir.exists(), "failed to create directory {}", dir.display());

        // intentionally not doing a shallow clone since that makes
        // incremental updates more exensive, however partial clones are a great
        // fit since that avoids fetching old parsers (which are not very useful)
        config.git(
            &[
                "clone",
                "--single-branch",
                "--filter=blob:none",
                "--branch",
                branch,
                remote,
                ".",
            ],
            &dir,
        )
    }
}

pub fn fetch(config: &Config, update_existing_grammar: bool) -> Result<()> {
    for repo in &config.repos {
        repo.fetch(config, update_existing_grammar)?
    }
    Ok(())
}

pub fn build_grammar(config: &Config, grammar: &str, force_rebuild: bool) -> Result<PathBuf> {
    for repo in &config.repos {
        if repo.has_grammar(config, grammar) {
            build::build_grammar(grammar, &repo.dir(config).join(grammar), force_rebuild)?;
            return Ok(repo
                .dir(config)
                .join(grammar)
                .join(grammar)
                .with_extension(LIB_EXTENSION));
        }
    }
    bail!("grammar not found in any configured repository")
}

pub fn list_grammars(config: &Config) -> Result<Vec<PathBuf>> {
    let mut res = Vec::new();
    for repo in &config.repos {
        res.append(&mut repo.list_grammars(config)?)
    }
    res.sort_by(|path1, path2| path1.file_name().cmp(&path2.file_name()));
    res.dedup_by(|path1, path2| path1.file_name() == path2.file_name());
    Ok(res)
}

pub fn build_all_grammars(
    config: &Config,
    force_rebuild: bool,
    concurrency: Option<NonZeroUsize>,
) -> Result<usize> {
    let grammars = list_grammars(config)?;
    let bar = ProgressBar::new(grammars.len() as u64).with_style(
        ProgressStyle::with_template("{spinner} {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
            .unwrap(),
    );
    bar.set_message("Compiling");
    bar.enable_steady_tick(Duration::from_millis(100));
    let i = AtomicUsize::new(0);
    let concurrency = concurrency
        .or_else(|| thread::available_parallelism().ok())
        .map_or(4, usize::from);
    let failed = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    let Some(grammar) = grammars.get(i.fetch_add(1, atomic::Ordering::Relaxed))
                    else {
                        break;
                    };
                    let name = grammar.file_name().unwrap().to_str().unwrap();
                    if let Err(err) = build::build_grammar(name, grammar, force_rebuild) {
                        for err in err.chain() {
                            bar.println(format!("error: {err}"))
                        }
                        failed.lock().unwrap().push(name.to_owned())
                    }
                    bar.inc(1);
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    if !failed.is_empty() {
        bail!("failed to build grammars {failed:?}")
    }
    Ok(grammars.len())
}

// TODO: version the metadata? Or allow unknown fields but warn on them?
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", untagged)]
pub enum Metadata {
    ParserDefinition(ParserDefinition),
    ReuseParser {
        /// The name of the grammar to reuse.
        /// Grammars should only be reused from the same `Repo`.
        #[serde(rename = "reuse-parser")]
        name: String,
    },
}

impl Metadata {
    pub fn parser_definition(self) -> Option<ParserDefinition> {
        match self {
            Self::ParserDefinition(parser_definition) => Some(parser_definition),
            Self::ReuseParser { .. } => None,
        }
    }

    pub fn read(path: &Path) -> Result<Metadata> {
        let json = fs::read_to_string(path)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| format!("invalid metadata.json file at {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(&self).unwrap();
        fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ParserDefinition {
    /// The git remote of the upstream grammar repository
    pub repo: String,
    /// The revision of the git remote when the files were imported
    pub rev: String,
    /// The SPDX license identifier of the upstream grammar repository
    #[serde(default)]
    pub license: String,
    /// Whether the `parser.c` file is compressed
    #[serde(default)]
    pub compressed: bool,
}

// ruzstd is a bit manual, if they provided a better Reader implementation this
// wouldn't be necessary... they don't do that because using zstd efficiently
// apparently requires a seekable reader. Most readers are seekable so just
// adding an extra trait bound would help... oh well

/// decompresses a file compressed by skidder
pub fn decompress(src: &mut File, mut dst: impl Write) -> Result<()> {
    const BATCH_SIZE: usize = 8 * 1024;
    let size = src.metadata()?.len();

    let mut src = BufReader::new(src);
    let mut decoder = FrameDecoder::new();
    let mut copy_buffer = [0; BATCH_SIZE];

    while src.stream_position()? < size {
        match decoder.reset(&mut src) {
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length: skip_size,
                ..
            })) => {
                src.seek(SeekFrom::Current(skip_size as i64)).unwrap();
                continue;
            }
            other => other?,
        }
        while !decoder.is_finished() {
            decoder.decode_blocks(&mut src, BlockDecodingStrategy::UptoBytes(BATCH_SIZE))?;
            while decoder.can_collect() > BATCH_SIZE {
                let read = decoder.read(&mut copy_buffer).unwrap();
                assert_eq!(read, BATCH_SIZE);
                dst.write_all(&copy_buffer)?;
            }
        }
        while decoder.can_collect() != 0 {
            let read = decoder.read(&mut copy_buffer).unwrap();
            dst.write_all(&copy_buffer[..read])?;
        }
    }
    Ok(())
}
