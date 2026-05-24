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
    #[arg(long, requires = "query")]
    details: bool,
}

fn main() {
    let cli = Cli::parse();

    let (mut registry, mut sources) = docs::Registry::load(&cli.paths);

    // If filesystem discovery yielded nothing, try loading from DB cache
    if registry.all_items().is_empty() {
        let cached = registry.merge_cached();
        sources.extend(cached);
    }

    match cli.query {
        Some(query) => {
            let results = registry.search(&query, 50);
            if cli.details {
                if let Some(item) = results.first() {
                    let doc = registry.load_doc_content(&item.html_rel);
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
