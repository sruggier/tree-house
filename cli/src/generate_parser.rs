use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use skidder::{Metadata, decompress};
use tempfile::TempDir;

use crate::collect_grammars;
use crate::flags::RegenerateParser;
use crate::import::import_compressed;

impl RegenerateParser {
    pub fn run(self) -> Result<()> {
        let paths = if self.recursive {
            collect_grammars(&self.path)?
        } else {
            vec![self.path.clone()]
        };
        let temp_dir =
            TempDir::new().context("failed to create temporary directory for decompression")?;
        // create dummy file to prevent TS CLI from creating a full skeleton
        File::create(temp_dir.path().join("grammar.js"))
            .context("failed to create temporary directory for decompression")?;
        let mut failed = Vec::new();
        for grammar_dir in paths {
            let grammar_name = grammar_dir.file_name().unwrap().to_str().unwrap();
            if grammar_name <= "dart" {
                continue;
            }
            println!("checking {grammar_name}");

            let compressed = Metadata::read(&grammar_dir.join("metadata.json"))
                .with_context(|| format!("failed to read metadata for {grammar_name}"))?
                .parser_definition()
                .unwrap()
                .compressed;

            let src_path = grammar_dir.join("src");
            let src_grammar_path = src_path.join("grammar.json");
            let grammar_path = temp_dir.path().join("grammar.json");
            if !src_grammar_path.exists() {
                eprintln!("grammar.json not found for {grammar_name}");
                failed.push(grammar_name.to_owned());
                continue;
            }
            if compressed {
                let dst = File::create(&grammar_path).with_context(|| {
                    format!(
                        "failed to create grammar.json file in temporary build directory {}",
                        temp_dir.path().display()
                    )
                })?;
                decompress_file(&src_grammar_path, dst).with_context(|| {
                    format!("failed to decompress grammar.json for {grammar_name}")
                })?;
            } else {
                fs::copy(src_grammar_path, &grammar_path)
                    .with_context(|| format!("failed to copy grammar.json for {grammar_name}"))?;
            }
            println!("running tree-sitter generate {}", grammar_path.display());
            let res = Command::new("tree-sitter")
                .arg("generate")
                .arg("--no-bindings")
                .arg(&grammar_path)
                .current_dir(temp_dir.path())
                .status()
                .with_context(|| {
                    format!(
                        "failed to execute tree-sitter generate {}",
                        grammar_path.display()
                    )
                })?
                .success();
            if !res {
                eprintln!(
                    "failed to execute tree-sitter generate {}",
                    grammar_path.display()
                );
                failed.push(grammar_name.to_owned());
                continue;
            }

            let new_parser_path = temp_dir.path().join("src").join("parser.c");
            let old_parser_path = src_path.join("parser.c");
            let mut old_parser = Vec::new();
            decompress_file(&old_parser_path, &mut old_parser)
                .with_context(|| format!("failed to decompress parser for {grammar_name}"))?;
            let old_parser = String::from_utf8_lossy(&old_parser);
            let new_parser = fs::read_to_string(&new_parser_path)
                .context("tree-sitter cli did not generate parser.c")?;
            if old_parser.trim() == new_parser.trim() {
                continue;
            }
            failed.push(grammar_name.to_owned());
            eprintln!("existing parser.c was outdated updating...");
            if compressed {
                import_compressed(&new_parser_path, &old_parser_path).with_context(|| {
                    format!("failed to compress new parser.c for {grammar_name}")
                })?;
            } else {
                fs::copy(&new_parser_path, &old_parser_path)
                    .with_context(|| format!("failed to opy new parser.c for {grammar_name}"))?;
            }
        }
        ensure!(
            failed.is_empty(),
            "parser.c files is not up to date for {failed:?}!"
        );
        Ok(())
    }
}

fn decompress_file(src: &Path, dst: impl Write) -> Result<()> {
    File::open(src)
        .map_err(anyhow::Error::from)
        .and_then(|mut reader| decompress(&mut reader, dst))
        .with_context(|| format!("failed to decompress {}", src.display()))?;
    Ok(())
}
