mod docs;
mod ui;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clidoc", about = "Browse Rust documentation in the terminal")]
struct Cli {
    /// Extra doc source paths (added on top of auto-discovered sources).
    /// Each can be a rustup doc root, cargo target/doc, or crate source dir.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Search query (non-interactive mode: prints matching items to stdout)
    #[arg(short, long)]
    query: Option<String>,

    /// Show full doc details for the first match (non-interactive mode)
    #[arg(long, short, requires = "query")]
    details: bool,
}

fn main() {
    let cli = Cli::parse();

    let (mut registry, mut sources) = docs::Registry::load(&cli.paths);

    // Always merge any cached sources not yet loaded.
    // This restores previously indexed crates even when filesystem discovery
    // didn't find them (e.g. old toolchains, moved projects).
    let cached = registry.merge_cached();
    sources.extend(cached);

    match cli.query {
        Some(query) => {
            let results = registry.search(&query, 50);
            if cli.details {
                if let Some(item) = results.first() {
                    let doc = registry.load_doc_content(&item.html_rel);
                    let doc = doc
                        .lines()
                        .filter_map(|l| cleanup_markdown(l))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .replace("\n\n", "\n");

                    let doc = if doc.len() > 0 && doc.chars().rev().next().unwrap() != '\n' {
                        format!("{}\n", doc)
                    } else {
                        doc
                    };

                    print!("{doc}");
                } else {
                    eprintln!("No matching items found.");
                    std::process::exit(1);
                }
            } else {
                for item in &results {
                    println!("{}", item.display_name());
                }
                if results.is_empty() {
                    std::process::exit(1);
                }
            }
        }
        None => {
            ui::run(registry, sources);
        }
    }
}

fn cleanup_markdown(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with("```") {
        return None;
    }
    if trimmed.starts_with("**") && trimmed.contains("Source") || trimmed == "Source" {
        return None;
    }
    Some(trimmed)
}
