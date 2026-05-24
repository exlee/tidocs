use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// A known doc source (a directory on disk that contains rustdoc HTML).
#[derive(Debug, Clone)]
pub struct DocSource {
    /// Unique identifier for this source in SQLite
    pub id: String,
    /// The filesystem path (canonicalized)
    pub path: PathBuf,
    /// Human-readable label
    pub label: String,
}

/// Registry of all doc items, loaded from SQLite cache or HTML files.
pub struct Registry {
    items: Vec<DocItem>,
    doc_roots: Vec<PathBuf>,
    content_cache: Arc<Mutex<HashMap<String, String>>>,
    runtime: tokio::runtime::Handle,
}

impl Registry {
    /// Load the registry from explicit doc roots, using per-source SQLite cache.
    pub fn load(doc_roots: &[PathBuf]) -> Self {
        let mut all_items = Vec::new();

        for root in doc_roots {
            let source_id = source_id_for_path(root);
            match db_load_items(&source_id) {
                Some(cached) => {
                    eprintln!("[clidoc] {} items from cache: {}", cached.len(), root.display());
                    all_items.extend(cached);
                }
                None => {
                    eprintln!("[clidoc] scanning {} ...", root.display());
                    let mut items = Vec::new();
                    load_from_html_dir(root, &mut items);
                    items.sort_by(|a, b| a.path.cmp(&b.path));
                    items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
                    eprintln!("[clidoc] found {} items in {}", items.len(), root.display());
                    db_save_items(&source_id, root, &items);
                    all_items.extend(items);
                }
            }
        }

        // Global dedup
        all_items.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        all_items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);

        eprintln!("[clidoc] total: {} items", all_items.len());
        Self::new(all_items, doc_roots.to_vec())
    }

    /// Load registry from all known doc sources (auto-discovery + SQLite).
    /// Returns (registry, list of active sources).
    pub fn load_all_known() -> (Self, Vec<DocSource>) {
        let sources = discover_doc_sources();
        eprintln!("[clidoc] discovered {} doc sources", sources.len());

        let mut all_items = Vec::new();
        let mut active_sources = Vec::new();

        for source in &sources {
            if db_needs_scan(&source.id, &source.path) {
                eprintln!("[clidoc] scanning {} ...", source.label);
                let mut items = Vec::new();
                load_from_html_dir(&source.path, &mut items);
                items.sort_by(|a, b| a.path.cmp(&b.path));
                items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
                db_save_items(&source.id, &source.path, &items);
                eprintln!("[clidoc] {} items from {}", items.len(), source.label);
                all_items.extend(items);
            } else {
                if let Some(cached) = db_load_items(&source.id) {
                    eprintln!("[clidoc] {} items from cache: {}", cached.len(), source.label);
                    all_items.extend(cached);
                }
            }
            active_sources.push(source.clone());
        }

        all_items.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        all_items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);

        eprintln!("[clidoc] total: {} items", all_items.len());
        let roots: Vec<PathBuf> = sources.iter().map(|s| s.path.clone()).collect();
        (Self::new(all_items, roots), active_sources)
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

    /// Add items from an extra doc root on top of existing items.
    pub fn with_extra_root(self, root: &Path) -> Self {
        let source_id = source_id_for_path(root);
        let extra = match db_load_items(&source_id) {
            Some(cached) => {
                eprintln!("[clidoc] {} extra items from cache: {}", cached.len(), root.display());
                cached
            }
            None => {
                eprintln!("[clidoc] scanning extra source {} ...", root.display());
                let mut items = Vec::new();
                load_from_html_dir(root, &mut items);
                items.sort_by(|a, b| a.path.cmp(&b.path));
                items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
                eprintln!("[clidoc] found {} extra items in {}", items.len(), root.display());
                db_save_items(&source_id, root, &items);
                items
            }
        };

        let mut all_items = self.items;
        all_items.extend(extra);
        // Dedup
        all_items.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        all_items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
        eprintln!("[clidoc] total: {} items", all_items.len());

        let mut roots = self.doc_roots;
        roots.push(root.to_path_buf());
        Self {
            items: all_items,
            doc_roots: roots,
            content_cache: self.content_cache,
            runtime: self.runtime,
        }
    }

    /// Load and cache doc content for an item by its html_rel path.
    pub fn load_doc_content(&self, html_rel: &str) -> String {
        if let Some(cached) = self.content_cache.lock().unwrap().get(html_rel) {
            return cached.clone();
        }
        let content = self.convert_html(html_rel);
        self.content_cache.lock().unwrap().insert(html_rel.to_string(), content.clone());
        content
    }

    /// Prefetch doc content asynchronously.
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

// ---------------------------------------------------------------------------
// SQLite cache (per-source, free functions)
// ---------------------------------------------------------------------------

fn db_ensure_tables(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
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
}

fn db_load_items(source_id: &str) -> Option<Vec<DocItem>> {
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("clidoc");
    fs::create_dir_all(&cache_dir).ok();
    let db_path = cache_dir.join("index.db");
    let conn = rusqlite::Connection::open(&db_path).ok()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
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

    let mut stmt = conn
        .prepare("SELECT path, kind, html_rel, desc FROM items WHERE source = ?1 ORDER BY path")
        .ok()?;
    let items: Vec<DocItem> = stmt
        .query_map(params![source_id], |row| {
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

fn db_needs_scan(source_id: &str, path: &Path) -> bool {
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("clidoc");
    fs::create_dir_all(&cache_dir).ok();
    let db_path = cache_dir.join("index.db");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return true,
    };
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources (id TEXT PRIMARY KEY, path TEXT NOT NULL, fingerprint TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS items (source TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, html_rel TEXT NOT NULL, desc TEXT, PRIMARY KEY (source, path, kind));"
    ).ok();

    let fp = match compute_source_fingerprint(path) {
        Some(fp) => fp,
        None => return true,
    };
    let stored: Option<String> = conn
        .query_row("SELECT fingerprint FROM sources WHERE id = ?1", params![source_id], |row| row.get(0))
        .ok();
    stored.as_deref() != Some(&fp)
}

fn db_save_items(source_id: &str, source_path: &Path, items: &[DocItem]) {
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("clidoc");
    fs::create_dir_all(&cache_dir).ok();
    let db_path = cache_dir.join("index.db");
    let mut conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    db_ensure_tables(&conn);

    let fp = match compute_source_fingerprint(source_path) {
        Some(fp) => fp,
        None => return,
    };
    let path_str = source_path.display().to_string();
    let _ = conn.execute(
        "INSERT OR REPLACE INTO sources (id, path, fingerprint) VALUES (?1, ?2, ?3)",
        params![source_id, &path_str, &fp],
    );
    let _ = conn.execute("DELETE FROM items WHERE source = ?1", params![source_id]);

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
            let _ = stmt.execute(params![source_id, &item.path, &item.kind, &item.html_rel, &item.desc]);
        }
    }
    let _ = tx.commit();
}

/// Compute fingerprint for a single doc root directory.
fn compute_source_fingerprint(path: &Path) -> Option<String> {
    let mut entries: Vec<(String, u64)> = Vec::new();

    // Top-level all.html if present (single-crate roots)
    let all_html = path.join("all.html");
    if all_html.exists() {
        let mtime = fs::metadata(&all_html).ok()?.modified().ok()?
            .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        entries.push(("all.html".to_string(), mtime));
    }

    // Top-level sidebar-items
    for name in &["sidebar-items.js", "sidebar-items1.87.0.js"] {
        let sb = path.join(name);
        if sb.exists() {
            let mtime = fs::metadata(&sb).ok()?.modified().ok()?
                .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            entries.push((name.to_string(), mtime));
        }
    }

    // Sub-directory sidebar-items + all.html (for multi-crate roots like rustup)
    if let Ok(rd) = fs::read_dir(path) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() { continue; }
            let dir_name = p.file_name()?.to_str()?.to_string();

            let sub_all = p.join("all.html");
            if sub_all.exists() {
                let mtime = fs::metadata(&sub_all).ok()?.modified().ok()?
                    .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                entries.push((format!("{dir_name}/all.html"), mtime));
            }

            for sb_name in &["sidebar-items.js", "sidebar-items1.87.0.js"] {
                let sb = p.join(sb_name);
                if sb.exists() {
                    let mtime = fs::metadata(&sb).ok()?.modified().ok()?
                        .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                    entries.push((format!("{dir_name}/{sb_name}"), mtime));
                }
            }
        }
    }

    if entries.is_empty() { return None; }

    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (rel, mtime) in &entries {
        rel.hash(&mut hasher);
        mtime.hash(&mut hasher);
    }
    Some(format!("{:x}", hasher.finish()))
}

/// Generate a unique source ID from a filesystem path.
pub fn source_id_for_path(path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("s:{:x}", hasher.finish())
}

/// Auto-discover doc sources from known locations.
pub fn discover_doc_sources() -> Vec<DocSource> {
    let mut sources = Vec::new();

    // 1. Rustup std docs
    let home = dirs::home_dir().unwrap_or_default();
    let rustup_base = home.join(".rustup/toolchains");
    let mut rustup_toolchain_names: Vec<String> = Vec::new();

    if rustup_base.is_dir() {
        if let Ok(rd) = fs::read_dir(&rustup_base) {
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let doc_dir = p.join("share/doc/rust/html");
                if doc_dir.join("all.html").exists() || has_crate_subdirs(&doc_dir) {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
                    rustup_toolchain_names.push(name.to_string());
                    sources.push(DocSource {
                        id: source_id_for_path(&doc_dir),
                        path: doc_dir,
                        label: format!("rustup/{name}"),
                    });
                }
            }
        }
        // Keep only the active toolchain
        if rustup_toolchain_names.len() > 1 {
            if let Ok(output) = std::process::Command::new("rustup").args(["show", "active-toolchain"]).output() {
                let active = String::from_utf8_lossy(&output.stdout)
                    .split_whitespace().next().unwrap_or("").to_string();
                sources.retain(|s| s.label.ends_with(&format!("/{active}")));
            }
        }
    }

    sources
}

/// Check if a directory is a multi-crate doc root (contains subdirectories with sidebar-items).
fn has_crate_subdirs(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().is_dir() && read_dir_has_sidebar(&e.path()))
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Multi-word search
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// HTML parsing / item extraction
// ---------------------------------------------------------------------------

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

    // Check if this is a multi-crate root
    let mut has_crate_subdirs = false;
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && read_dir_has_sidebar(&path) {
                has_crate_subdirs = true;
                let crate_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                load_crate(&path, &crate_name, items);
            }
        }
    }

    if !has_crate_subdirs {
        let crate_name = base
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        load_crate(base, &crate_name, items);
    }

    items.sort_by(|a, b| a.path.cmp(&b.path));
    items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
}

fn load_crate(base: &Path, crate_name: &str, items: &mut Vec<DocItem>) {
    let crate_dir_name = base
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(crate_name);

    load_all_html(base, crate_name, crate_dir_name, items);
    load_methods(base, items);

    // Recurse into subdirectories with sidebar-items
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "." && n != "..") {
                if read_dir_has_sidebar(&path) {
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

fn load_methods(base: &Path, items: &mut Vec<DocItem>) {
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

    let re = regex::Regex::new(
        r###"<h4 class="code-header">.*?href="#(method|tymethod|associatedconstant)\.([a-z_][a-z0-9_]*)".*?class="(fn|const)">.*?</h4>"###,
    )
    .expect("invalid regex");

    let file_root = base.parent().unwrap_or(base);

    for (parent_path, html_rel) in method_holders {
        let html_path = file_root.join(&html_rel);
        let Ok(raw) = fs::read_to_string(&html_path) else {
            continue;
        };

        for cap in re.captures_iter(&raw) {
            let anchor_type = &cap[1];
            let name = &cap[2];
            let css_class = &cap[3];

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

fn load_all_html(base: &Path, crate_name: &str, crate_dir_name: &str, items: &mut Vec<DocItem>) {
    let all_html = base.join("all.html");
    if !all_html.exists() {
        return;
    }
    let Ok(content) = fs::read_to_string(&all_html) else {
        return;
    };

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

fn extract_all_sections(html: &str) -> Vec<(String, usize)> {
    let mut sections = Vec::new();
    let mut pos = 0;
    while pos < html.len() {
        if let Some(start) = html[pos..].find("<h3") {
            pos += start;
            let section_kind = detect_section_kind(&html[pos..]).unwrap_or("mod".to_string());
            sections.push((section_kind, pos));
            pos += 4;
        } else {
            break;
        }
    }
    sections
}

fn extract_links_in_section(html: &str, section_start: usize) -> Vec<(String, String)> {
    let body = &html[section_start..];
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
                if !href.contains(".") && !href.contains("/") && !href.contains("../") {
                    pos = href_start + end;
                    continue;
                }
                let after_href = &section[href_start + end..];
                if let Some(text_start) = after_href.find('>') {
                    let text_area = &after_href[text_start + 1..];
                    if let Some(text_end) = text_area.find("</a>") {
                        let raw_text = text_area[..text_end].trim();
                        let text = strip_html_tags(raw_text).trim().to_string();
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

fn load_submodule(dir: &Path, crate_name: &str, crate_dir_name: &str, module_path: &str, items: &mut Vec<DocItem>) {
    let sidebar = load_sidebar(dir);

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

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && read_dir_has_sidebar(&path) {
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

fn build_html_rel_in_crate(dir: &Path, crate_dir_name: &str, kind: &str, name: &str) -> String {
    let mod_dir_rel = path_relative_to_crate_dir(dir, crate_dir_name);
    let suffix = html_filename_for_kind(kind, name);
    format!("{}/{}", mod_dir_rel, suffix)
}

fn build_submodule_html_rel(dir: &Path, crate_dir_name: &str, href: &str) -> String {
    let dir_rel = path_relative_to_crate_dir(dir, crate_dir_name);
    format!("{}/{}", dir_rel, href)
}

fn path_relative_to_crate_dir(dir: &Path, crate_dir_name: &str) -> String {
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if dir_name == crate_dir_name {
        crate_dir_name.to_string()
    } else {
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

    let nbsp = "\u{00a0}";
    output
        .replace(&format!("{nbsp}Copy item path"), "")
        .replace(" Copy item path", "")
        .replace("**Expand description**", "")
        .replace(" Expand description", "")
        .replace(&format!("{nbsp}Expand description"), "")
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
