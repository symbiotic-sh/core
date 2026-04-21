//! CLI wrapper for the vault linter.
//!
//! Subcommands:
//!   file <path>     — lint a single Markdown file
//!   dir  <path>     — recursively lint all .md files in a directory
//!   stdin           — read Markdown from stdin and lint it
//!
//! Flags:
//!   --format text   — human-readable output (default: json)
//!
//! Exit codes:
//!   0 — no errors found
//!   1 — lint errors found
//!   2 — usage / IO error

use std::io::Read;
use std::path::Path;
use std::process;

use symbiotic_memory::vault_layout::{
    collect_canonical_markdown_files, is_generated_artifact_path,
};
use symbiotic_memory::vault_linter::{lint_content, lint_file, LintReport};

/// A single finding emitted by the linter.
#[derive(serde::Serialize)]
struct Finding {
    file: Option<String>,
    level: &'static str,
    message: String,
}

fn findings_from_report(report: &LintReport, file: Option<&str>) -> Vec<Finding> {
    let mut out = Vec::new();
    for e in &report.errors {
        out.push(Finding {
            file: file.map(String::from),
            level: "error",
            message: e.clone(),
        });
    }
    for w in &report.warnings {
        out.push(Finding {
            file: file.map(String::from),
            level: "warning",
            message: w.clone(),
        });
    }
    out
}

fn print_findings(findings: &[Finding], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(findings).expect("serialize findings");
            println!("{json}");
        }
        OutputFormat::Text => {
            if findings.is_empty() {
                println!("No findings.");
            } else {
                for f in findings {
                    let loc = f.file.as_deref().unwrap_or("<stdin>");
                    println!("[{}] {}: {}", f.level.to_uppercase(), loc, f.message);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum OutputFormat {
    Json,
    Text,
}

fn usage() -> ! {
    eprintln!(
        "Usage: symbiotic-linter [--format json|text] <subcommand> <args>\n\
         \n\
         Subcommands:\n\
         \n\
         \x20 file <path>    Lint a single Markdown file\n\
         \x20 dir  <path>    Lint all .md files in a directory (recursive)\n\
         \x20 stdin          Read Markdown from stdin and lint it\n\
         \n\
         Options:\n\
         \x20 --format json  JSON array output (default)\n\
         \x20 --format text  Human-readable output"
    );
    process::exit(2);
}

fn collect_md_files(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    if dir.join("ledger").exists()
        || dir.join("identity").exists()
        || dir.join("operations").exists()
    {
        let mut files = collect_canonical_markdown_files(dir)?
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        files.sort();
        return Ok(files);
    }
    if dir.file_name().and_then(|name| name.to_str()) == Some("ledger") {
        let mut files = Vec::new();
        collect_ledger_entity_files_recursive(dir, &mut files)?;
        files.sort();
        return Ok(files);
    }
    let mut files = Vec::new();
    collect_md_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_md_files_recursive(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_md_files_recursive(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_ledger_entity_files_recursive(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_ledger_entity_files_recursive(&path, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if is_generated_artifact_path(&path) {
            continue;
        }
        let Some(file_stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(parent_name) = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        if file_stem == parent_name {
            out.push(path);
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }

    // Parse optional --format flag
    let mut format = OutputFormat::Json;
    let mut rest = args.as_slice();

    if rest.len() >= 2 && rest[0] == "--format" {
        match rest[1].as_str() {
            "json" => format = OutputFormat::Json,
            "text" => format = OutputFormat::Text,
            other => {
                eprintln!("Unknown format: {other}");
                usage();
            }
        }
        rest = &rest[2..];
    }

    if rest.is_empty() {
        usage();
    }

    let subcommand = rest[0].as_str();
    let sub_args = &rest[1..];

    match subcommand {
        "file" => {
            if sub_args.len() != 1 {
                eprintln!("file subcommand requires exactly one path argument");
                usage();
            }
            let path = Path::new(&sub_args[0]);
            match lint_file(path) {
                Ok(report) => {
                    let findings = findings_from_report(&report, Some(&sub_args[0]));
                    print_findings(&findings, format);
                    if !report.is_valid() {
                        process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error reading {}: {e}", sub_args[0]);
                    process::exit(2);
                }
            }
        }
        "dir" => {
            if sub_args.len() != 1 {
                eprintln!("dir subcommand requires exactly one path argument");
                usage();
            }
            let dir = Path::new(&sub_args[0]);
            if !dir.is_dir() {
                eprintln!("Not a directory: {}", sub_args[0]);
                process::exit(2);
            }
            let files = match collect_md_files(dir) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Error reading directory {}: {e}", sub_args[0]);
                    process::exit(2);
                }
            };
            if files.is_empty() {
                eprintln!("No .md files found in {}", sub_args[0]);
                print_findings(&[], format);
                return;
            }
            let mut all_findings = Vec::new();
            let mut has_errors = false;
            for file in &files {
                match lint_file(file) {
                    Ok(report) => {
                        if !report.is_valid() {
                            has_errors = true;
                        }
                        let path_str = file.to_string_lossy();
                        all_findings.extend(findings_from_report(&report, Some(&path_str)));
                    }
                    Err(e) => {
                        let path_str = file.to_string_lossy().to_string();
                        eprintln!("Error reading {path_str}: {e}");
                        all_findings.push(Finding {
                            file: Some(path_str),
                            level: "error",
                            message: format!("IO error: {e}"),
                        });
                        has_errors = true;
                    }
                }
            }
            print_findings(&all_findings, format);
            if has_errors {
                process::exit(1);
            }
        }
        "stdin" => {
            if !sub_args.is_empty() {
                eprintln!("stdin subcommand takes no arguments");
                usage();
            }
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("Error reading stdin: {e}");
                process::exit(2);
            }
            let report = lint_content(&buf);
            let findings = findings_from_report(&report, None);
            print_findings(&findings, format);
            if !report.is_valid() {
                process::exit(1);
            }
        }
        other => {
            eprintln!("Unknown subcommand: {other}");
            usage();
        }
    }
}
