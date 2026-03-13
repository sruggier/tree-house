use std::env::current_dir;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use skidder::{Metadata, ParserDefinition};
use walkdir::WalkDir;

use crate::flags::Import;
const LICENSE_FILE_NAMES: &[&str] = &["LICENSE", "LICENSE.txt", "LICENCE", "LICENCE", "COPYING"];
const LICENSE_SEARCH: &[(&str, &str)] = &[
    ("unlicense", "unlicense"),
    ("EUROPEAN UNION PUBLIC LICENCE v. 1.2", "EUPL-1.2"),
    ("The Artistic License 2.0", "Artistic-2.0"),
    ("Apache License", "Apache-2.0"),
    ("GNU GENERAL PUBLIC LICENSE", "GPL-3.0"),
    ("MIT License", "MIT"),
    ("DO WHAT THE FUCK YOU WANT TO PUBLIC LICENSE", "WTFPL"),
    ("BSD 3-Clause License", "BSD-3-Clause"),
];

impl Import {
    fn repo(&self) -> Result<PathBuf> {
        match &self.repo {
            Some(path) => Ok(path.clone()),
            None => Ok(current_dir()?),
        }
    }

    pub fn run(self) -> Result<()> {
        let repo = self.repo()?;
        for path in &self.path {
            let Some(dir_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
                bail!("invalid path {path:?}");
            };
            let mut src_path = path.to_owned();
            let grammar_name = match dir_name.rsplit_once(':') {
                Some((dir_name, grammar_name)) => {
                    src_path.set_file_name(dir_name);
                    grammar_name
                }
                None => dir_name,
            };
            src_path.push("src");
            let mut dst_path = repo.join(grammar_name);
            fs::create_dir_all(&dst_path)
                .with_context(|| format!("failed to create {}", dst_path.display()))?;
            if !src_path.join("parser.c").exists() {
                eprintln!(
                    "skipping grammar {grammar_name}: no parser.c found at {}!",
                    src_path.display()
                );
                continue;
            }
            src_path.pop();
            println!("importing {grammar_name}");
            for dir in ["src", "../common"] {
                let src_path = src_path.join(dir);
                if !src_path.exists() {
                    continue;
                }
                dst_path.push(dir.strip_prefix("../").unwrap_or(dir));
                for file in WalkDir::new(&src_path) {
                    let file = file?;
                    if !file.file_type().is_file() {
                        continue;
                    }
                    let Some(file_name) = file.file_name().to_str() else {
                        continue;
                    };
                    let Some((_, extension)) = file_name.rsplit_once('.') else {
                        continue;
                    };
                    if !(matches!(extension, "h" | "c" | "cc")
                        || extension == "scm" && self.import_queries
                        || file_name == "grammar.json")
                        || file_name.starts_with("parser_abi") && extension == "c"
                    {
                        continue;
                    }
                    let relative_path = file.path().strip_prefix(&src_path).unwrap();
                    let dst_path = dst_path.join(relative_path);
                    fs::create_dir_all(dst_path.parent().unwrap()).with_context(|| {
                        format!("failed to create {}", dst_path.parent().unwrap().display())
                    })?;
                    let res = if matches!(file_name, "parser.c" | "grammar.json")
                        && file.path().parent() == Some(&src_path)
                        && dir == "src"
                    {
                        import_compressed(file.path(), &dst_path)?;
                        continue;
                    } else if matches!(extension, "h" | "c" | "cc")
                        && src_path.join("../../common").exists()
                    {
                        fs::read_to_string(file.path()).and_then(|contents| {
                            let contents = contents.replace("../../common/", "../common/");
                            fs::write(&dst_path, contents)
                        })
                    } else {
                        fs::copy(file.path(), &dst_path).map(|_| ())
                    };
                    res.with_context(|| {
                        format!(
                            "failed to copy {} to {}",
                            file.path().display(),
                            dst_path.display()
                        )
                    })?;
                }
                dst_path.pop();
            }
            let license_file = LICENSE_FILE_NAMES
                .iter()
                .map(|name| src_path.join(name))
                .find(|src_path| src_path.exists());
            let mut license = None;
            if let Some(license_file) = license_file {
                let license_file_content = fs::read_to_string(&license_file)
                    .with_context(|| format!("failed to read {}", license_file.display()))?;
                fs::write(dst_path.join("LICENSE"), &license_file_content).with_context(|| {
                    format!("failed to write {}", dst_path.join("LICENSE").display())
                })?;
                license = LICENSE_SEARCH
                    .iter()
                    .find(|(needle, _)| license_file_content.contains(needle))
                    .map(|(_, license)| (*license).to_owned());
                if license.is_none() {
                    eprintln!("failed to identify license in {}", license_file.display());
                }
            } else {
                eprintln!("warning: {grammar_name} does not have a LICENSE file!");
            }
            if self.metadata {
                let metadata_path = dst_path.join("metadata.json");
                let rev =
                    git_output(&["rev-parse", "HEAD"], &src_path, false).with_context(|| {
                        format!("failed to obtain git revision at {}", src_path.display())
                    })?;
                let repo = git_output(&["remote", "get-url", "origin"], &src_path, false)
                    .with_context(|| {
                        format!("failed to obtain git remote at {}", src_path.display())
                    })?;
                let package_metadata: Option<PackageJson> =
                    fs::read_to_string(src_path.join("package.json"))
                        .ok()
                        .and_then(|json| serde_json::from_str(&json).ok());
                if let Some(package_metadata) = package_metadata {
                    match &license {
                        Some(license) if license != &package_metadata.license => eprintln!(
                            "warning: license in package identifier differs from detected license {license} != {}",
                            &package_metadata.license
                        ),
                        _ => license = Some(package_metadata.license),
                    }
                }

                let old_metadata = Metadata::read(&metadata_path)
                    .ok()
                    .and_then(Metadata::parser_definition)
                    .filter(|old_meta| old_meta.repo == repo && !old_meta.license.is_empty());

                if let Some(old_metadata) = &old_metadata {
                    match &license {
                        Some(license) => {
                            if license != &old_metadata.license {
                                eprintln!(
                                    "warning: license has changed {} => {license}",
                                    old_metadata.license
                                );
                            }
                        }
                        None => {
                            eprintln!(
                                "warning: couldn't determine license for {grammar_name}, keeping {:?}",
                                old_metadata.license
                            );
                            license = Some(old_metadata.license.clone())
                        }
                    }
                }
                if license.is_none() {
                    eprintln!("warning: couldn't import determine license for {grammar_name}",);
                }

                let metadata = Metadata::ParserDefinition(ParserDefinition {
                    repo,
                    rev,
                    license: license.unwrap_or_default(),
                    compressed: true,
                });
                metadata.write(&metadata_path).with_context(|| {
                    format!(
                        "failed to write metadata.json to {}",
                        metadata_path.display()
                    )
                })?
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct PackageJson {
    license: String,
}

fn git_output(args: &[&str], dir: &Path, verbose: bool) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    if verbose {
        println!("{}: git {}", dir.display(), args.join(" "))
    }
    let res = cmd.output().context("failed to invoke git")?;
    if !res.status.success() {
        let _ = io::stdout().write_all(&res.stdout);
        let _ = io::stderr().write_all(&res.stderr);
        bail!("git returned non-zero exit-code: {}", res.status);
    }
    String::from_utf8(res.stdout)
        .context("git returned invalid utf8")
        .map(|output| output.trim_end().to_string())
}

pub fn import_compressed(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let success = Command::new("zstd")
        .args(["--ultra", "-22", "-f", "-o"])
        .arg(dst)
        .arg(src)
        .status()
        .with_context(|| format!("failed to compress {}", src.display()))?
        .success();
    ensure!(success, "failed to compress {}", src.display());
    Ok(())
}
