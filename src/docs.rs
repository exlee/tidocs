use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;

use rusqlite::params;

/// A single documentation item (module, struct, fn, trait, etc.)
#[derive(Debug, Clone)]
pub struct DocItem {
    /// Display name like "std::io::Read" or "std::vec::Vec::push"
    pub path: String,
    /// Kind: mod, struct, fn, trait, enum, type, constant, macro, method, assoc_type, assoc_const, primitive, keyword
    pub kind: String,
    /// Relative path from the doc root HTML dir to the item's HTML file
    pub html_rel: String,
    /// Brief description (if available)
    #[allow(dead_code)]
    pub desc: Option<String>,
}

impl DocItem {
    pub fn display_name(&self) -> String {
        let prefix = match self.kind.as_ref() {
            "fn" | "method" => "fn ",
            "trait" => "trait ",
            "struct" => "struct ",
            "enum" => "enum ",
            "mod" => "mod ",
            "macro" => "macro! ",
            "type" | "assoc_type" => "type ",
            "constant" | "const" | "assoc_const" => "const ",
            "primitive" => "type ",
            "keyword" => "keyword ",
            _ => "",
        };
        format!("{}{} ({})", prefix, self.path, self.kind)
    }
}

/// Registry of all doc items, loaded from sidebar-items JS and all.html files
pub struct Registry {
    items: Vec<DocItem>,
    doc_roots: Vec<PathBuf>,
    content_cache: Arc<Mutex<HashMap<String, String>>>,
    runtime: Handle,
}

impl Registry {
    pub fn load(doc_roots: &[PathBuf]) -> Self {
        eprintln!("[clidoc] loading docs...");
        let items = match Self::load_from_cache(doc_roots) {
            Some(cached) => {
                eprintln!("[clidoc] loaded {} items from cache", cached.len());
                cached
            }
            None => {
                eprintln!("[clidoc] cache miss, scanning HTML files...");
                let mut items = Vec::new();
                for root in doc_roots {
                    eprintln!("[clidoc] scanning {}", root.display());
                    load_from_html_dir(root, &mut items);
                }
                eprintln!("[clidoc] found {} items, saving to cache", items.len());
                items.sort_by(|a, b| a.path.cmp(&b.path));
                items.dedup_by(|a, b| a.path == b.path);
                Self::save_to_cache(doc_roots, &items);
                items
            }
        };
        eprintln!("[clidoc] starting TUI with {} items", items.len());
        Self::new(items, doc_roots.to_vec())
    }

    fn new(items: Vec<DocItem>, doc_roots: Vec<PathBuf>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        Self {
            items,
            doc_roots,
            content_cache: Arc::new(Mutex::new(HashMap::new())),
            runtime: handle,
        }
    }

    fn db_path() -> Option<PathBuf> {
        let cache_dir = dirs::cache_dir()?.join("clidoc");
        fs::create_dir_all(&cache_dir).ok()?;
        Some(cache_dir.join("index.db"))
    }

    /// Compute a fingerprint for the doc root by hashing sidebar-items and all.html mtimes.
    fn compute_fingerprint(doc_roots: &[PathBuf]) -> Option<String> {
        let mut entries: Vec<(String, u64)> = Vec::new();
        for root in doc_roots {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(read_dir) = fs::read_dir(&dir) else { continue };
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let name = path.file_name()?.to_str()?.to_string();
                    if name.starts_with("sidebar-items") || name == "all.html" {
                        let mtime = entry.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                        let rel = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
                        entries.push((format!("{:?}:{}", root, rel), mtime));
                    }
                }
            }
        }
        entries.sort();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (rel, mtime) in &entries {
            rel.hash(&mut hasher);
            mtime.hash(&mut hasher);
        }
        Some(format!("{:x}", hasher.finish()))
    }

    fn load_from_cache(doc_roots: &[PathBuf]) -> Option<Vec<DocItem>> {
        let db_path = Self::db_path()?;
        let conn = rusqlite::Connection::open(&db_path).ok()?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sources (
                doc_root TEXT PRIMARY KEY,
                fingerprint TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS items (
                source TEXT NOT NULL,
                path TEXT NOT NULL,
                kind TEXT NOT NULL,
                html_rel TEXT NOT NULL,
                desc TEXT,
                PRIMARY KEY (source, path, kind)
            );"
        ).ok()?;

        let fingerprint = Self::compute_fingerprint(doc_roots)?;
        let roots_key: String = doc_roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(":");

        let stored_fp: Option<String> = conn
            .query_row(
                "SELECT fingerprint FROM sources WHERE doc_root = ?1",
                params![&roots_key],
                |row| row.get(0),
            )
            .ok();

        if stored_fp.as_deref() != Some(&fingerprint) {
            return None;
        }

        let mut stmt = conn
            .prepare(
                "SELECT path, kind, html_rel, desc FROM items WHERE source = ?1 ORDER BY path",
            )
            .ok()?;

        let items: Vec<DocItem> = stmt
            .query_map(params![&roots_key], |row| {
                Ok(DocItem {
                    path: row.get(0)?,
                    kind: row.get(1)?,
                    html_rel: row.get(2)?,
                    desc: row.get(3)?,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        if items.is_empty() { None } else { Some(items) }
    }

    fn save_to_cache(doc_roots: &[PathBuf], items: &[DocItem]) {
        let db_path = match Self::db_path() {
            Some(p) => p,
            None => return,
        };
        let mut conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sources (
                doc_root TEXT PRIMARY KEY,
                fingerprint TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS items (
                source TEXT NOT NULL,
                path TEXT NOT NULL,
                kind TEXT NOT NULL,
                html_rel TEXT NOT NULL,
                desc TEXT,
                PRIMARY KEY (source, path, kind)
            );"
        );

        let fingerprint = match Self::compute_fingerprint(doc_roots) {
            Some(fp) => fp,
            None => return,
        };

        let roots_key: String = doc_roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(":");

        let _ = conn.execute(
            "INSERT OR REPLACE INTO sources (doc_root, fingerprint) VALUES (?1, ?2)",
            params![&roots_key, &fingerprint],
        );

        let _ = conn.execute(
            "DELETE FROM items WHERE source = ?1",
            params![&roots_key],
        );

        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(_) => return,
        };
        {
            let mut stmt = match tx.prepare(
                "INSERT INTO items (source, path, kind, html_rel, desc) VALUES (?1, ?2, ?3, ?4, ?5)"
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            for item in items {
                let _ = stmt.execute(params![
                    &roots_key,
                    &item.path,
                    &item.kind,
                    &item.html_rel,
                    &item.desc,
                ]);
            }
        }
        let _ = tx.commit();
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<&DocItem> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut scored: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| {
                let score = match_item_score(&item.path, &words)?;
                Some((item, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.path.cmp(&b.0.path)));
        scored.truncate(limit);
        scored.into_iter().map(|(item, _)| item).collect()
    }

    pub fn all_items(&self) -> &[DocItem] {
        &self.items
    }

    /// Load and cache doc content for an item by its html_rel path.
    pub fn load_doc_content(&self, html_rel: &str) -> String {
        // Check cache first
        if let Some(cached) = self.content_cache.lock().unwrap().get(html_rel) {
            return cached.clone();
        }
        let content = self.convert_html(html_rel);
        self.content_cache.lock().unwrap().insert(html_rel.to_string(), content.clone());
        content
    }

    /// Prefetch doc content asynchronously. Safe to call repeatedly --
    /// already-cached items are skipped cheaply.
    pub fn prefetch(&self, html_rels: Vec<String>) {
        let cache = Arc::clone(&self.content_cache);
        let roots = self.doc_roots.clone();
        self.runtime.spawn(async move {
            for rel in html_rels {
                {
                    let guard = cache.lock().unwrap();
                    if guard.contains_key(&rel) {
                        continue;
                    }
                }
                let file_rel = rel.split('#').next().unwrap_or("").to_string();
                let mut loaded = false;
                for root in &roots {
                    let html_path = root.join(&file_rel);
                    if !html_path.exists() {
                        continue;
                    }
                    let Ok(raw) = fs::read_to_string(&html_path) else {
                        continue;
                    };
                    let content = render_doc_page(&raw);
                    cache.lock().unwrap().insert(rel.clone(), content);
                    loaded = true;
                    break;
                }
                if !loaded {
                    cache.lock().unwrap().insert(rel, format!("Documentation not found: {}", file_rel));
                }
            }
        });
    }

    fn convert_html(&self, html_rel: &str) -> String {
        let file_rel = html_rel.split('#').next().unwrap_or(html_rel);
        for root in &self.doc_roots {
            let html_path = root.join(file_rel);
            if !html_path.exists() {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&html_path) else {
                continue;
            };
            return render_doc_page(&raw);
        }
        format!("Documentation file not found: {}", file_rel)
    }
}

/// Multi-word substring match. Each word must appear as a case-insensitive substring.
/// Returns a score (higher = better) or None if any word is missing.
pub(crate) fn match_item_score(path: &str, words: &[&str]) -> Option<i32> {
    let path_lower = path.to_lowercase();
    let mut total_score: i32 = 0;
    for &word in words {
        let word_lower = word.to_lowercase();
        let pos = path_lower.find(&word_lower)?;
        if pos == 0 {
            total_score += 100;
        } else {
            let prev = path_lower.as_bytes()[pos - 1];
            if prev == b':' || prev == b'_' {
                total_score += 80;
            } else {
                total_score += 10;
            }
        }
    }
    total_score -= (path.len() / 5) as i32;
    Some(total_score)
}

/// Check if a directory contains a sidebar-items JS file
fn read_dir_has_sidebar(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("sidebar-items"))
        })
        .unwrap_or(false)
}

/// Recursively load all items from the HTML doc directory.
fn load_from_html_dir(base: &Path, items: &mut Vec<DocItem>) {
    if !base.exists() || !base.is_dir() {
        return;
    }

    // Check if this is a multi-crate root (like rustup's html/ dir)
    // by looking for subdirectories with top-level sidebar-items
    let mut has_crate_subdirs = false;
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let has_sidebar = read_dir_has_sidebar(&path);
                if has_sidebar {
                    has_crate_subdirs = true;
                    // This is a crate directory
                    let crate_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    load_crate(&path, &crate_name, items);
                }
            }
        }
    }

    // If no crate subdirs found, treat base itself as a single crate
    if !has_crate_subdirs {
        let crate_name = base
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        load_crate(base, &crate_name, items);
    }

    // Deduplicate by path
    items.sort_by(|a, b| a.path.cmp(&b.path));
    items.dedup_by(|a, b| a.path == b.path);
}

/// Load all items for a single crate from its doc HTML directory.
/// `crate_dir_name` is the directory name (e.g., "std") used to prefix HTML paths.
fn load_crate(base: &Path, crate_name: &str, items: &mut Vec<DocItem>) {
    let crate_dir_name = base
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(crate_name);

    // Parse all.html for the full flat listing
    load_all_html(base, crate_name, crate_dir_name, items);

    // Extract methods from individual HTML pages
    load_methods(base, items);

    // Recurse into subdirectories that have sidebar-items
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "." && n != "..") {
                let has_sidebar = read_dir_has_sidebar(&path);
                if has_sidebar {
                    let sub_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    load_submodule(&path, crate_name, crate_dir_name, sub_name, items);
                }
            }
        }
    }
}

/// Extract methods and associated items from individual HTML pages.
/// For each struct/enum/trait/primitive item, scan its HTML file for
/// `<h4 class="code-header">` entries and create child items.
fn load_methods(base: &Path, items: &mut Vec<DocItem>) {
    // Collect paths of items that can have methods
    let method_holders: Vec<(String, String)> = items
        .iter()
        .filter(|item| {
            matches!(
                item.kind.as_str(),
                "struct" | "enum" | "trait" | "primitive" | "union"
            )
        })
        .map(|item| (item.path.clone(), item.html_rel.clone()))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut new_items: Vec<DocItem> = Vec::new();

    // Regex matches <h4 class="code-header"> entries and extracts:
    //   group 1: anchor type (method|tymethod|associatedconstant)
    //   group 2: method name
    //   group 3: CSS class (fn|const)
    let re = regex::Regex::new(
        r###"<h4 class="code-header">.*?href="#(method|tymethod|associatedconstant)\.([a-z_][a-z0-9_]*)".*?class="(fn|const)">.*?</h4>"###,
    )
    .expect("invalid regex");

    // Extract methods from individual HTML pages.
    // html_rel includes crate prefix (e.g. "std/vec/struct.Vec.html"),
    // so we need to resolve against the parent of the crate directory.
    let file_root = base.parent().unwrap_or(base);

    for (parent_path, html_rel) in method_holders {
        let html_path = file_root.join(&html_rel);
        let Ok(raw) = fs::read_to_string(&html_path) else {
            continue;
        };

        for cap in re.captures_iter(&raw) {
            let anchor_type = &cap[1]; // method, tymethod, associatedconstant
            let name = &cap[2];
            let css_class = &cap[3]; // fn, const

            let kind = match css_class {
                "fn" => "method",
                "const" => "assoc_const",
                _ => continue,
            };

            if !seen.insert(format!("{parent_path}::{name}")) {
                continue;
            }

            let frag = format!("#{anchor_type}.{name}");
            let child_html_rel = format!("{html_rel}{frag}");

            new_items.push(DocItem {
                path: format!("{parent_path}::{name}"),
                kind: kind.to_string(),
                html_rel: child_html_rel,
                desc: None,
            });
        }
    }

    items.extend(new_items);
}

/// Parse all.html to extract all documented items with their hrefs.
fn load_all_html(base: &Path, crate_name: &str, crate_dir_name: &str, items: &mut Vec<DocItem>) {
    let all_html = base.join("all.html");
    if !all_html.exists() {
        return;
    }
    let Ok(content) = fs::read_to_string(&all_html) else {
        return;
    };

    // First detect sections from h3 headers (works across all rustdoc versions)
    for (kind, pos) in extract_all_sections(&content) {
        for (href, text) in extract_links_in_section(&content, pos) {
            let full_path = format!("{}::{}", crate_name, text);
            let html_rel = format!("{}/{}", crate_dir_name, href);
            items.push(DocItem {
                path: full_path,
                kind: kind.clone(),
                html_rel,
                desc: None,
            });
        }
    }
}

/// Strip HTML tags from a string, returning only the text content.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Extract (kind, position) pairs from section headers in all.html.
fn extract_all_sections(html: &str) -> Vec<(String, usize)> {
    let mut sections = Vec::new();
    let mut pos = 0;
    while pos < html.len() {
        if let Some(start) = html[pos..].find("<h3") {
            pos += start;
            // Extract section kind from the h3
            let section_kind = detect_section_kind(&html[pos..]).unwrap_or("mod".to_string());
            sections.push((section_kind, pos));
            pos += 4;
        } else {
            break;
        }
    }
    sections
}

/// Extract all links from the HTML body between `section_start` (an h3 position) and the next h3 or end.
fn extract_links_in_section(html: &str, section_start: usize) -> Vec<(String, String)> {
    let body = &html[section_start..];
    // Find the next <h3 to know our boundary
    let boundary = if let Some(h3) = body.find("<h3") {
        if h3 > 0 { h3 } else { body.len() }
    } else {
        body.len()
    };
    let section = &body[..boundary];

    let mut links = Vec::new();
    let mut pos = 0;
    while pos < section.len() {
        if let Some(start) = section[pos..].find("href=\"") {
            let href_start = pos + start + 6;
            if let Some(end) = section[href_start..].find('"') {
                let href = &section[href_start..href_start + end];
                // Skip non-doc links (no extension, no slash, relative navigation)
                if !href.contains(".") && !href.contains("/") && !href.contains("../") {
                    pos = href_start + end;
                    continue;
                }
                // Look for the link text
                let after_href = &section[href_start + end..];
                if let Some(text_start) = after_href.find('>') {
                    let text_area = &after_href[text_start + 1..];
                    if let Some(text_end) = text_area.find("</a>") {
                        let raw_text = text_area[..text_end].trim();
                        let text = strip_html_tags(raw_text).trim().to_string();
                        // Only include links that look like doc items
                        if !text.is_empty() && href.ends_with(".html") {
                            links.push((href.to_string(), text));
                            pos = href_start + end + text_start + 1 + text_end + 4;
                            continue;
                        }
                    }
                }
                pos = href_start + end;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    links
}

fn detect_section_kind(line: &str) -> Option<String> {
    let patterns: &[(&str, &str)] = &[
        ("modules", "mod"),
        ("structs", "struct"),
        ("enums", "enum"),
        ("functions", "fn"),
        ("traits", "trait"),
        ("type-aliases", "type"),
        ("types", "type"),
        ("constants", "const"),
        ("macros", "macro"),
        ("primitives", "primitive"),
        ("keywords", "keyword"),
        ("trait-aliases", "trait"),
        ("reexports", "reexport"),
        ("methods", "method"),
        ("associated-types", "assoc_type"),
        ("associated-consts", "assoc_const"),
    ];

    let line_lower = line.to_lowercase();
    for (id, kind) in patterns {
        if line_lower.contains(&format!("id=\"{}\"", id)) {
            return Some(kind.to_string());
        }
    }

    let heading_kinds: &[(&str, &str)] = &[
        ("re-exports", "reexport"),
        ("modules", "mod"),
        ("structs", "struct"),
        ("enums", "enum"),
        ("functions", "fn"),
        ("traits", "trait"),
        ("type aliases", "type"),
        ("constants", "const"),
        ("macros", "macro"),
        ("primitive types", "primitive"),
        ("keywords", "keyword"),
        ("trait aliases", "trait"),
    ];

    if let Some(start) = line_lower.find("<h3")
        && let Some(end) = line_lower[start..].find("</h3>")
    {
        let heading = &line_lower[start..start + end];
        for (text, kind) in heading_kinds {
            if heading.contains(text) {
                return Some(kind.to_string());
            }
        }
    }

    None
}

/// Recursively load items from sub-modules.
/// `module_path` is the full path from crate root (e.g., "io::buffered").
/// `crate_dir_name` is the directory name used for HTML path prefix.
fn load_submodule(dir: &Path, crate_name: &str, crate_dir_name: &str, module_path: &str, items: &mut Vec<DocItem>) {
    // Load sidebar items for this submodule
    let sidebar = load_sidebar(dir);

    // Build items from sidebar data
    for (kind, names) in &sidebar {
        for name in names {
            let item_path = if module_path.is_empty() {
                format!("{}::{}", crate_name, name)
            } else {
                format!("{}::{}::{}", crate_name, module_path, name)
            };
            let rel_in_crate = build_html_rel_in_crate(dir, crate_dir_name, kind, name);
            items.push(DocItem {
                path: item_path,
                kind: kind.clone(),
                html_rel: rel_in_crate,
                desc: None,
            });
        }
    }

    // Also load all.html if present (catches everything)
    let all_html = dir.join("all.html");
    if all_html.exists()
        && let Ok(content) = fs::read_to_string(&all_html)
    {
        for (kind, pos) in extract_all_sections(&content) {
            for (href, text) in extract_links_in_section(&content, pos) {
                let item_path = if module_path.is_empty() {
                    format!("{}::{}", crate_name, text)
                } else {
                    format!("{}::{}::{}", crate_name, module_path, text)
                };
                let html_rel = build_submodule_html_rel(dir, crate_dir_name, &href);
                items.push(DocItem {
                    path: item_path,
                    kind: kind.clone(),
                    html_rel,
                    desc: None,
                });
            }
        }
    }

    // Recurse deeper
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let has_sidebar = read_dir_has_sidebar(&path);
                if has_sidebar {
                    let sub_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    let sub_path = if module_path.is_empty() {
                        sub_name.to_string()
                    } else {
                        format!("{}::{}", module_path, sub_name)
                    };
                    load_submodule(&path, crate_name, crate_dir_name, &sub_path, items);
                }
            }
        }
    }
}

fn load_sidebar(dir: &Path) -> HashMap<String, Vec<String>> {
    let mut result = HashMap::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("sidebar-items") && name.ends_with(".js")
                && let Ok(content) = fs::read_to_string(entry.path())
            {
                parse_sidebar_items_js(&content, &mut result);
            }
        }
    }
    result
}

fn parse_sidebar_items_js(content: &str, out: &mut HashMap<String, Vec<String>>) {
    let Some(eq_pos) = content.find('=') else {
        return;
    };
    let json_str = content[eq_pos + 1..]
        .trim()
        .trim_end_matches(';')
        .trim();
    if let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, Vec<String>>>(
        json_str,
    ) {
        for (kind, names) in map {
            out.insert(kind, names);
        }
    }
}

/// Build the relative HTML path from the doc root to an item in a submodule.
/// `dir` is the current submodule directory.
fn build_html_rel_in_crate(dir: &Path, crate_dir_name: &str, kind: &str, name: &str) -> String {
    let mod_dir_rel = path_relative_to_crate_dir(dir, crate_dir_name);
    let suffix = html_filename_for_kind(kind, name);
    format!("{}/{}", mod_dir_rel, suffix)
}

/// Build the relative HTML path from the doc root for an item in a submodule all.html.
fn build_submodule_html_rel(dir: &Path, crate_dir_name: &str, href: &str) -> String {
    let dir_rel = path_relative_to_crate_dir(dir, crate_dir_name);
    format!("{}/{}", dir_rel, href)
}

/// Get the relative path of a module directory from the crate doc directory.
fn path_relative_to_crate_dir(dir: &Path, crate_dir_name: &str) -> String {
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if dir_name == crate_dir_name {
        crate_dir_name.to_string()
    } else {
        // Walk up from dir to find the crate dir and build relative path
        let mut components = Vec::new();
        let mut current = dir;
        loop {
            let name = current
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if name == crate_dir_name {
                components.reverse();
                return format!("{}/{}", crate_dir_name, components.join("/"));
            }
            components.push(name);
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                components.reverse();
                return components.join("/");
            }
        }
    }
}

fn html_filename_for_kind(kind: &str, name: &str) -> String {
    match kind {
        "struct" => format!("struct.{}.html", name),
        "enum" => format!("enum.{}.html", name),
        "fn" => format!("fn.{}.html", name),
        "trait" => format!("trait.{}.html", name),
        "type" => format!("type.{}.html", name),
        "const" | "constant" => format!("constant.{}.html", name),
        "macro" => format!("macro.{}.html", name),
        "mod" => format!("{}/index.html", name),
        "primitive" => format!("primitive.{}.html", name),
        _ => format!("{}.html", name),
    }
}

/// Extract the <main> content from rustdoc HTML and render as readable text
pub fn render_doc_page(html: &str) -> String {
    let main_content = if let Some(start) = html.find("<main>") {
        let after_main = &html[start + 6..];
        if let Some(end) = after_main.find("</main>") {
            &after_main[..end]
        } else {
            html
        }
    } else {
        html
    };

    match html_to_markdown_rs::convert(main_content, None) {
        Ok(result) => clean_rendered_text(result.content.as_deref().unwrap_or("")),
        Err(e) => format!("Failed to render HTML: {e}"),
    }
}

fn clean_rendered_text(rendered: &str) -> String {
    let lines: Vec<&str> = rendered.lines().collect();
    let mut output = String::new();
    let mut prev_blank = false;

    for line in &lines {
        let trimmed = line.trim();
        // Skip rustdoc chrome
        if trimmed == "Copy item path"
            || trimmed == "Expand description"
            || trimmed == "Run code"
            || (trimmed.starts_with("[](https://play.rust-lang.org") && trimmed.ends_with(')'))
            || (trimmed.starts_with("[](https://doc.rust-lang.org") && trimmed.ends_with(')'))
        {
            continue;
        }
        if trimmed.is_empty() {
            if !prev_blank {
                output.push('\n');
                prev_blank = true;
            }
        } else {
            if prev_blank && !output.is_empty() {
                output.push('\n');
            }
            output.push_str(trimmed);
            output.push('\n');
            prev_blank = false;
        }
    }

    // Clean up rustdoc chrome embedded in markdown text
    // Note: rustdoc uses non-breaking spaces (U+00A0) in some places
    let nbsp = "\u{00a0}";
    output
        .replace(&format!("{nbsp}Copy item path"), "")
        .replace(" Copy item path", "")
        .replace("**Expand description**", "")
        .replace(" Expand description", "")
        .replace(&format!("{nbsp}Expand description"), "")
}

/// Find the default doc root (rustup docs)
pub fn default_doc_root() -> PathBuf {
    let toolchain_output = std::process::Command::new("rustup")
        .args(["show", "active-toolchain"])
        .output()
        .ok();
    let toolchain = toolchain_output
        .as_ref()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split_whitespace().next().map(|s| s.to_string())
        })
        .unwrap_or_else(|| "stable".to_string());

    let home = dirs::home_dir().unwrap_or_default();
    let rustup_doc = home
        .join(".rustup/toolchains")
        .join(toolchain)
        .join("share/doc/rust/html");

    if rustup_doc.exists() {
        rustup_doc
    } else {
        eprintln!(
            "Could not find Rust docs at {}\nPass a path explicitly.",
            rustup_doc.display()
        );
        std::process::exit(1);
    }
}

/// Discover the doc HTML root from a given path.
pub fn discover_doc_root(path: &Path) -> PathBuf {
    if path.join("all.html").exists() {
        return path.to_path_buf();
    }

    if path.join("index.html").exists() {
        let has_sidebar = fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.starts_with("sidebar-items") || e.path().is_dir()
                    })
            })
            .unwrap_or(false);
        if has_sidebar {
            return path.to_path_buf();
        }
    }

    let target_doc = path.join("target/doc");
    if target_doc.exists() {
        return target_doc;
    }

    if path.join("Cargo.toml").exists() {
        let td = path.join("target/doc");
        if td.exists() {
            return td;
        }
        eprintln!("Hint: run 'cargo doc' in {} first", path.display());
        std::process::exit(1);
    }

    path.to_path_buf()
}
