mod docs;
mod ui;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clidoc", about = "Browse Rust documentation in the terminal")]
struct Cli {
    /// Doc source path (rustup doc root, cargo target/doc, or crate source dir)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Search query (non-interactive mode: prints matching items to stdout)
    #[arg(short, long)]
    query: Option<String>,

    /// Show full doc details for the first match (non-interactive mode)
    #[arg(long, requires = "query")]
    details: bool,
}

fn main() {
    let cli = Cli::parse();

    let mut doc_roots = Vec::new();

    match cli.path {
        Some(p) => doc_roots.push(docs::discover_doc_root(&p)),
        None => {
            doc_roots.push(docs::default_doc_root());
            // Auto-merge local target/doc if present in cwd
            let local = std::env::current_dir().unwrap_or_default().join("target/doc");
            if local.is_dir() {
                doc_roots.push(local);
            }
        }
    }

    let registry = docs::Registry::load(&doc_roots);

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
            ui::run(registry, doc_roots);
        }
    }
}
