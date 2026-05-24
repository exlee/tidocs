use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
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

/// A known doc source: a crate's documentation at a specific version.
#[derive(Debug, Clone)]
pub struct DocSource {
    /// Library name (e.g. "std", "serde", "tokio")
    pub lib_name: String,
    /// Version string (e.g. "1.87.0", "0.1.0")
    pub version: String,
    /// Filesystem path to the crate's HTML doc directory
    pub path: PathBuf,
}

impl DocSource {
    pub fn label(&self) -> String {
        format!("{} v{}", self.lib_name, self.version)
    }
}

/// Registry of all doc items, loaded from SQLite cache or HTML files.
pub struct Registry {
    items: Vec<DocItem>,
    doc_roots: Vec<PathBuf>,
    /// Maps crate dir prefix (e.g. "std") to (lib_name, version) for DB lookups.
    source_map: HashMap<String, (String, String)>,
    content_cache: Arc<Mutex<HashMap<String, String>>>,
    runtime: tokio::runtime::Handle,
}

impl Registry {
    /// Load registry from auto-discovered sources + optional extra paths.
    pub fn load(extra_paths: &[PathBuf]) -> (Self, Vec<DocSource>) {
        let mut sources = discover_doc_sources();

        // Add extra paths
        for p in extra_paths {
            let root = discover_doc_root(p);
            let mut found = false;
            if root.join("all.html").exists() || has_crate_subdirs(&root) {
                // Multi-crate root: enumerate each crate subdirectory
                if let Ok(rd) = fs::read_dir(&root) {
                    for entry in rd.flatten() {
                        let sub = entry.path();
                        if sub.is_dir() && read_dir_has_sidebar(&sub) {
                            let name = sub.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                            let ver = detect_version(&sub).unwrap_or_else(|| "0.0.0".to_string());
                            sources.push(DocSource { lib_name: name, version: ver, path: sub });
                            found = true;
                        }
                    }
                }
            }
            if !found && read_dir_has_sidebar(&root) {
                // Single crate root
                let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                let ver = detect_version(&root).unwrap_or_else(|| "0.0.0".to_string());
                sources.push(DocSource { lib_name: name, version: ver, path: root });
            }
        }

        let mut all_items = Vec::new();
        let mut active_sources = Vec::new();
        // Maps crate-dir-prefix (first component of html_rel) -> (lib_name, version)
        let mut source_map: HashMap<String, (String, String)> = HashMap::new();

        for source in &sources {
            let source_key = source_key(&source.lib_name, &source.version);
            let crate_dir_prefix = source.path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            source_map.insert(crate_dir_prefix.clone(), (source.lib_name.clone(), source.version.clone()));

            if db_needs_scan(&source.lib_name, &source.version, &source.path) {
                let mut items = Vec::new();
                load_crate(&source.path, &source.lib_name, &mut items);
                items.sort_by(|a, b| a.path.cmp(&b.path));
                items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
                db_save_items(&source_key, &source.path, &source.lib_name, &source.version, &items);
                // Cache HTML pages into DB
                cache_pages_for_items(&crate_dir_prefix, &source.lib_name, &source.version, &source.path, &items);
                all_items.extend(items);
            } else {
                if let Some(cached) = db_load_items(&source_key) {
                    all_items.extend(cached);
                }
            }
            active_sources.push(source.clone());
        }

        all_items.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        all_items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
        eprintln!("[clidoc] total: {} items", all_items.len());

        let roots: Vec<PathBuf> = active_sources.iter().map(|s| s.path.clone()).collect();
        (Self::new(all_items, roots, source_map), active_sources)
    }

    fn new(items: Vec<DocItem>, doc_roots: Vec<PathBuf>, source_map: HashMap<String, (String, String)>) -> Self {
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
            source_map,
            content_cache: Arc::new(Mutex::new(HashMap::new())),
            runtime: handle,
        }
    }

    /// Merge cached sources from DB into this registry.
    /// Used when filesystem discovery yields no sources but DB has cached data.
    pub fn merge_cached(&mut self) -> Vec<DocSource> {
        let conn = match rusqlite::Connection::open(&db_path()) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        db_ensure_tables(&conn);

        let mut stmt = match conn.prepare(
            "SELECT lib_name, version FROM sources"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows: Vec<(String, String)> = stmt
            .query_map(params![], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap_or_else(|_| panic!("failed to query sources"))
            .filter_map(|r| r.ok())
            .collect();

        let mut new_sources = Vec::new();
        for (lib_name, version) in rows {
            // Skip if already loaded
            if self.source_map.values().any(|(l, v)| l == &lib_name && v == &version) {
                continue;
            }
            let key = source_key(&lib_name, &version);
            if let Some(items) = db_load_items(&key) {
                let crate_prefix = lib_name.clone();
                self.source_map.insert(crate_prefix, (lib_name.clone(), version.clone()));
                self.items.extend(items);
                let path_str: String = conn.query_row(
                    "SELECT path FROM sources WHERE lib_name = ?1 AND version = ?2",
                    params![&lib_name, &version],
                    |row| row.get(0),
                ).unwrap_or_default();
                let path = PathBuf::from(&path_str);
                let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
                if !self.doc_roots.contains(&parent) {
                    self.doc_roots.push(parent);
                }
                new_sources.push(DocSource {
                    lib_name,
                    version,
                    path,
                });
            }
        }

        self.items.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
        self.items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
        if !new_sources.is_empty() {
            // Already printed total above; just note what was added
        }
        new_sources
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<&DocItem> {
        // Check for leading kind badge (e.g. "fn peek", "st HashMap")
        let (kind_filter, rest_query) = if let Some(space_pos) = query.find(' ') {
            let badge = &query[..space_pos];
            let kinds = kind_badge_to_kinds(badge);
            if !kinds.is_empty() {
                (Some(kinds), &query[space_pos + 1..])
            } else {
                (None, query)
            }
        } else {
            (None, query)
        };
        let words: Vec<&str> = rest_query.split_whitespace().collect();
        let case_sensitive = has_uppercase(rest_query);
        let mut scored: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| {
                if let Some(ref kf) = kind_filter {
                    if !kf.contains(&item.kind) {
                        return None;
                    }
                }
                let score = match_item_score(&item.path, &item.kind, &words, case_sensitive)?;
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

    pub fn load_doc_content(&self, html_rel: &str) -> String {
        if let Some(cached) = self.content_cache.lock().unwrap().get(html_rel) {
            return cached.clone();
        }
        // Resolve which source this belongs to via the first path component
        let file_rel = html_rel.split('#').next().unwrap_or(html_rel);
        let crate_prefix = file_rel.split('/').next().unwrap_or("");

        // Try DB cache first
        let content = if let Some((lib_name, version)) = self.source_map.get(crate_prefix) {
            db_load_page_html(lib_name, version, html_rel)
                .as_deref()
                .map(|s| render_doc_page(s))
        } else {
            None
        };

        let content = content.unwrap_or_else(|| self.convert_html(html_rel));
        self.content_cache.lock().unwrap().insert(html_rel.to_string(), content.clone());
        content
    }

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
                'roots: for root in &roots {
                    let html_path = root.join(&file_rel);
                    if !html_path.exists() { continue; }
                    let Ok(raw) = fs::read_to_string(&html_path) else { continue; };
                    let content = render_doc_page(&raw);
                    cache.lock().unwrap().insert(rel.clone(), content);
                    break 'roots;
                }
                // Don't insert "not found" poison into cache.
                // If we can't resolve it now, let on-demand load try DB or filesystem.
            }
        });
    }

    fn convert_html(&self, html_rel: &str) -> String {
        let file_rel = html_rel.split('#').next().unwrap_or(html_rel);
        for root in &self.doc_roots {
            let html_path = root.join(file_rel);
            if !html_path.exists() { continue; }
            let Ok(raw) = fs::read_to_string(&html_path) else { continue; };
            return render_doc_page(&raw);
        }
        format!("Documentation file not found: {}", file_rel)
    }
}

// ---------------------------------------------------------------------------
// SQLite: sources keyed by (lib_name, version)
// ---------------------------------------------------------------------------

fn db_path() -> PathBuf {
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("clidoc");
    fs::create_dir_all(&cache_dir).ok();
    cache_dir.join("index.db")
}

fn source_key(lib_name: &str, version: &str) -> String {
    format!("{}@{}", lib_name, version)
}

fn db_ensure_tables(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources (
            lib_name  TEXT NOT NULL,
            version   TEXT NOT NULL,
            path      TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            PRIMARY KEY (lib_name, version)
        );
        CREATE TABLE IF NOT EXISTS items (
            lib_name TEXT NOT NULL,
            version  TEXT NOT NULL,
            path     TEXT NOT NULL,
            kind     TEXT NOT NULL,
            html_rel TEXT NOT NULL,
            desc     TEXT,
            PRIMARY KEY (lib_name, version, path, kind)
        );
        CREATE TABLE IF NOT EXISTS pages (
            lib_name TEXT NOT NULL,
            version  TEXT NOT NULL,
            html_rel TEXT NOT NULL,
            html     BLOB NOT NULL,
            PRIMARY KEY (lib_name, version, html_rel)
        );"
    );
}

/// Compress a string with zlib deflate.
fn compress(text: &str) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(text.as_bytes()).ok()?
    ;
    Some(encoder.finish().ok()?)
}

/// Decompress zlib-deflated bytes back to string.
fn decompress(data: &[u8]) -> Option<String> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    String::from_utf8(out).ok()
}

/// Load raw HTML from DB cache (compressed).
fn db_load_page_html(lib_name: &str, version: &str, html_rel: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(&db_path()).ok()?;
    db_ensure_tables(&conn);
    let file_rel = html_rel.split('#').next().unwrap_or(html_rel);
    let mut stmt = conn.prepare(
        "SELECT html FROM pages WHERE lib_name = ?1 AND version = ?2 AND html_rel = ?3"
    ).ok()?;
    let data: Vec<u8> = stmt.query_row(params![lib_name, version, file_rel], |row| row.get(0)).ok()?;
    decompress(&data)
}

/// Cache all unique HTML pages referenced by items.
fn cache_pages_for_items(_crate_prefix: &str, lib_name: &str, version: &str, crate_dir: &Path, items: &[DocItem]) {
    // Collect unique base HTML files (strip fragment anchors)
    let mut seen: HashSet<&str> = HashSet::new();
    let mut html_files: Vec<&str> = Vec::new();
    for item in items {
        let file_rel = item.html_rel.split('#').next().unwrap_or(&item.html_rel);
        if seen.insert(file_rel) {
            html_files.push(file_rel);
        }
    }

    // Resolve against the doc root (parent of crate dir)
    let doc_root = crate_dir.parent().unwrap_or(crate_dir);

    let mut conn = match rusqlite::Connection::open(&db_path()) {
        Ok(c) => c,
        Err(_) => return,
    };
    db_ensure_tables(&conn);

    // Delete old pages for this source
    let _ = conn.execute(
        "DELETE FROM pages WHERE lib_name = ?1 AND version = ?2",
        params![lib_name, version],
    );

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(_) => return,
    };
    {
        let mut stmt = match tx.prepare(
            "INSERT INTO pages (lib_name, version, html_rel, html) VALUES (?1, ?2, ?3, ?4)"
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        for file_rel in html_files {
            let path = doc_root.join(file_rel);
            let Ok(raw) = fs::read_to_string(&path) else { continue; };
            let compressed = match compress(&raw) {
                Some(c) => c,
                None => continue,
            };
            let _ = stmt.execute(params![lib_name, version, file_rel, compressed]);
        }
    }
    let _ = tx.commit();
}

fn compute_fingerprint(crate_dir: &Path) -> Option<String> {
    let mut entries: Vec<(String, u64)> = Vec::new();

    let all = crate_dir.join("all.html");
    if all.exists() {
        let mtime = fs::metadata(&all).ok()?.modified().ok()?
            .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        entries.push(("all.html".into(), mtime));
    }

    if let Ok(rd) = fs::read_dir(crate_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() { continue; }
            let name = p.file_name()?.to_str()?.to_string();
            if name.starts_with("sidebar-items") && name.ends_with(".js") {
                let mtime = fs::metadata(&p).ok()?.modified().ok()?
                    .duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                entries.push((name, mtime));
            }
        }
    }

    if entries.is_empty() { return None; }

    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (name, mtime) in &entries {
        name.hash(&mut hasher);
        mtime.hash(&mut hasher);
    }
    Some(format!("{:x}", hasher.finish()))
}

fn db_needs_scan(lib_name: &str, version: &str, crate_dir: &Path) -> bool {
    let conn = match rusqlite::Connection::open(&db_path()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    db_ensure_tables(&conn);

    let fp = match compute_fingerprint(crate_dir) {
        Some(fp) => fp,
        None => {
            // Files not on disk -- check if we have cached data to fall back on
            let has_cache: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sources WHERE lib_name = ?1 AND version = ?2)",
                params![lib_name, version],
                |row| row.get(0),
            ).unwrap_or(false);
            return !has_cache; // needs_scan=false if cache exists
        }
    };

    let stored: Option<String> = conn
        .query_row(
            "SELECT fingerprint FROM sources WHERE lib_name = ?1 AND version = ?2",
            params![lib_name, version],
            |row| row.get(0),
        )
        .ok();

    // Also check if the path changed (docs rebuilt in a different location)
    if let Ok(path_str) = conn.query_row::<String, _, _>(
        "SELECT path FROM sources WHERE lib_name = ?1 AND version = ?2",
        params![lib_name, version],
        |row| row.get(0),
    ) {
        if path_str != crate_dir.display().to_string() {
            return true; // path changed, must rescan
        }
    }

    stored.as_deref() != Some(&fp)
}

fn db_load_items(key: &str) -> Option<Vec<DocItem>> {
    let conn = rusqlite::Connection::open(&db_path()).ok()?;
    db_ensure_tables(&conn);

    let (lib_name, version) = key.split_once('@')?;

    let mut stmt = conn.prepare(
        "SELECT path, kind, html_rel, desc FROM items WHERE lib_name = ?1 AND version = ?2 ORDER BY path"
    ).ok()?;

    let items: Vec<DocItem> = stmt
        .query_map(params![lib_name, version], |row| {
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

fn db_save_items(_key: &str, crate_dir: &Path, lib_name: &str, version: &str, items: &[DocItem]) {
    let mut conn = match rusqlite::Connection::open(&db_path()) {
        Ok(c) => c,
        Err(_) => return,
    };
    db_ensure_tables(&conn);

    let fp = match compute_fingerprint(crate_dir) {
        Some(fp) => fp,
        None => return,
    };

    let path_str = crate_dir.display().to_string();
    let _ = conn.execute(
        "INSERT OR REPLACE INTO sources (lib_name, version, path, fingerprint) VALUES (?1, ?2, ?3, ?4)",
        params![lib_name, version, &path_str, &fp],
    );
    let _ = conn.execute(
        "DELETE FROM items WHERE lib_name = ?1 AND version = ?2",
        params![lib_name, version],
    );

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(_) => return,
    };
    {
        let mut stmt = match tx.prepare(
            "INSERT INTO items (lib_name, version, path, kind, html_rel, desc) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        for item in items {
            let _ = stmt.execute(params![lib_name, version, &item.path, &item.kind, &item.html_rel, &item.desc]);
        }
    }
    let _ = tx.commit();
}

// ---------------------------------------------------------------------------
// Version detection
// ---------------------------------------------------------------------------

/// Try to detect the version of a crate from its doc directory.
fn detect_version(crate_dir: &Path) -> Option<String> {
    // Method 1: Look for version in the crate's index.html
    let index_html = crate_dir.join("index.html");
    if let Ok(content) = fs::read_to_string(&index_html) {
        // rustdoc often includes "1.0.0" in a <meta> or heading
        if let Some(pos) = content.find("<nav class=\"sub\">") {
            let nav = &content[pos..];
            if let Some(end) = nav.find("</nav>") {
                let nav_content = &nav[..end];
                // Look for version pattern like "1.87.0"
                let re = regex::Regex::new(r"(\d+\.\d+\.\d+)").ok()?;
                if let Some(cap) = re.captures(nav_content) {
                    return Some(cap[1].to_string());
                }
            }
        }
    }

    // Method 2: Walk up to find Cargo.toml and parse version
    let mut dir = crate_dir;
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            if let Ok(content) = fs::read_to_string(&toml) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("version") {
                        if let Some(rest) = trimmed.strip_prefix("version") {
                            let rest = rest.trim_start_matches('=').trim();
                            let ver = rest.trim_start_matches('"').trim_end_matches('"').trim();
                            if !ver.is_empty() {
                                return Some(ver.to_string());
                            }
                        }
                    }
                }
            }
            break;
        }
        dir = match dir.parent() {
            Some(p) => p,
            None => break,
        };
    }

    None
}

/// Get the rustc version for rustup doc sources.
fn rustc_version() -> String {
    let output = std::process::Command::new("rustc")
        .args(["--version"])
        .output()
        .ok();
    let s = output
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    // Parse "rustc 1.87.0 (..." -> "1.87.0"
    let re = regex::Regex::new(r"(\d+\.\d+\.\d+)").unwrap();
    re.captures(&s)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "0.0.0".to_string())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Auto-discover doc sources from known locations.
fn discover_doc_sources() -> Vec<DocSource> {
    let mut sources = Vec::new();
    let home = dirs::home_dir().unwrap_or_default();

    // Rustup toolchain docs: each sub-crate (std, core, alloc, etc.) is a separate source
    let rustup_base = home.join(".rustup/toolchains");
    let mut toolchains: Vec<(PathBuf, String)> = Vec::new();

    if rustup_base.is_dir() {
        if let Ok(rd) = fs::read_dir(&rustup_base) {
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_dir() { continue; }
                let doc_dir = p.join("share/doc/rust/html");
                if !doc_dir.is_dir() { continue; }
                let tc_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                toolchains.push((doc_dir, tc_name));
            }
        }

        // Keep only active toolchain
        if toolchains.len() > 1 {
            if let Ok(output) = std::process::Command::new("rustup").args(["show", "active-toolchain"]).output() {
                let active = String::from_utf8_lossy(&output.stdout)
                    .split_whitespace().next().unwrap_or("").to_string();
                toolchains.retain(|(_, name)| name == &active);
            }
        }

        // Determine version once (same for all crates in a toolchain)
        let ver = rustc_version();

        for (doc_dir, _) in &toolchains {
            if let Ok(rd) = fs::read_dir(doc_dir) {
                for entry in rd.flatten() {
                    let sub = entry.path();
                    if !sub.is_dir() || !read_dir_has_sidebar(&sub) { continue; }
                    let crate_name = sub.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                    sources.push(DocSource {
                        lib_name: crate_name,
                        version: ver.clone(),
                        path: sub,
                    });
                }
            }
        }
    }

    sources
}

// ---------------------------------------------------------------------------
// Multi-word search
// ---------------------------------------------------------------------------

/// Multi-word substring match. Each word must appear as a case-insensitive substring.
/// Returns a score (higher = better) or None if any word is missing.
/// Returns true if the query contains any uppercase ASCII letter.
/// Map a 2-letter kind badge back to internal kind strings.
/// Returns owned Strings for easy ownership transfer.
pub(crate) fn kind_badge_to_kinds(badge: &str) -> Vec<String> {
    match badge {
        "fn" => vec!["fn".into(), "method".into()],
        "tr" => vec!["trait".into()],
        "st" => vec!["struct".into()],
        "en" => vec!["enum".into()],
        "md" => vec!["mod".into()],
        "ma" => vec!["macro".into()],
        "ty" => vec!["type".into(), "assoc_type".into()],
        "co" => vec!["const".into(), "constant".into(), "assoc_const".into()],
        "pr" => vec!["primitive".into()],
        "kw" => vec!["keyword".into()],
        ">>" => vec!["reexport".into()],
        _ => vec![],
    }
}

/// Returns true if the query contains any uppercase ASCII letter.
pub(crate) fn has_uppercase(query: &str) -> bool {
    query.chars().any(|c| c.is_ascii_uppercase())
}

pub(crate) fn match_item_score(path: &str, _kind: &str, words: &[&str], case_sensitive: bool) -> Option<i32> {
    let (search_path, needs_lower) = if case_sensitive {
        (Cow::Borrowed(path), false)
    } else {
        (Cow::Owned(path.to_lowercase()), true)
    };
    let mut total_score: i32 = 0;
    for &word in words {
        let search_word = if needs_lower {
            Cow::Owned(word.to_lowercase())
        } else {
            Cow::Borrowed(word)
        };
        let pos = search_path.find(search_word.as_ref())?;

        // Bonus: if case-insensitive mode but the matched slice in the original
        // path already matches the query word's exact casing, it's a better hit.
        let case_match_bonus = if !case_sensitive {
            let matched_in_path = &path[pos..pos + word.len()];
            if matched_in_path == word {
                // Exact case match despite insensitive search — strong signal
                50
            } else {
                0
            }
        } else {
            0
        };

        if pos == 0 {
            total_score += 100 + case_match_bonus;
        } else {
            let prev = search_path.as_bytes()[pos - 1];
            if prev == b':' || prev == b'_' {
                total_score += 80 + case_match_bonus;
            } else {
                total_score += 10 + case_match_bonus;
            }
        }
    }
    total_score -= (path.len() / 5) as i32;
    Some(total_score)
}

// ---------------------------------------------------------------------------
// HTML parsing / item extraction
// ---------------------------------------------------------------------------

fn read_dir_has_sidebar(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("sidebar-items"))
        })
        .unwrap_or(false)
}

fn has_crate_subdirs(dir: &Path) -> bool {
    if !dir.is_dir() { return false; }
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().is_dir() && read_dir_has_sidebar(&e.path()))
        })
        .unwrap_or(false)
}

/// Load all items for a single crate from its doc HTML directory.
fn load_crate(base: &Path, crate_name: &str, items: &mut Vec<DocItem>) {
    if !base.exists() || !base.is_dir() { return; }

    let crate_dir_name = base.file_name().and_then(|n| n.to_str()).unwrap_or(crate_name);

    load_all_html(base, crate_name, crate_dir_name, items);
    load_methods(base, items);

    // Recurse into subdirectories with sidebar-items
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "." && n != "..") {
                if read_dir_has_sidebar(&path) {
                    let sub_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    load_submodule(&path, crate_name, crate_dir_name, sub_name, items);
                }
            }
        }
    }
}

fn load_methods(base: &Path, items: &mut Vec<DocItem>) {
    let method_holders: Vec<(String, String)> = items
        .iter()
        .filter(|item| matches!(item.kind.as_str(), "struct" | "enum" | "trait" | "primitive" | "union"))
        .map(|item| (item.path.clone(), item.html_rel.clone()))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut new_items: Vec<DocItem> = Vec::new();

    let re = regex::Regex::new(
        r###"<h4 class="code-header">.*?href="#(method|tymethod|associatedconstant)\.([a-z_][a-z0-9_]*)".*?class="(fn|const)">.*?</h4>"###,
    ).expect("invalid regex");

    let file_root = base.parent().unwrap_or(base);

    for (parent_path, html_rel) in method_holders {
        let html_path = file_root.join(&html_rel);
        let Ok(raw) = fs::read_to_string(&html_path) else { continue; };

        for cap in re.captures_iter(&raw) {
            let anchor_type = &cap[1];
            let name = &cap[2];
            let css_class = &cap[3];
            let kind = match css_class {
                "fn" => "method",
                "const" => "assoc_const",
                _ => continue,
            };
            if !seen.insert(format!("{parent_path}::{name}")) { continue; }
            let frag = format!("#{anchor_type}.{name}");
            new_items.push(DocItem {
                path: format!("{parent_path}::{name}"),
                kind: kind.to_string(),
                html_rel: format!("{html_rel}{frag}"),
                desc: None,
            });
        }
    }
    items.extend(new_items);
}

fn load_all_html(base: &Path, crate_name: &str, crate_dir_name: &str, items: &mut Vec<DocItem>) {
    let all_html = base.join("all.html");
    if !all_html.exists() { return; }
    let Ok(content) = fs::read_to_string(&all_html) else { return; };

    for (kind, pos) in extract_all_sections(&content) {
        for (href, text) in extract_links_in_section(&content, pos) {
            items.push(DocItem {
                path: format!("{}::{}", crate_name, text),
                kind: kind.clone(),
                html_rel: format!("{}/{}", crate_dir_name, href),
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
        ("modules", "mod"), ("structs", "struct"), ("enums", "enum"),
        ("functions", "fn"), ("traits", "trait"), ("type-aliases", "type"),
        ("types", "type"), ("constants", "const"), ("macros", "macro"),
        ("primitives", "primitive"), ("keywords", "keyword"),
        ("trait-aliases", "trait"), ("reexports", "reexport"),
        ("methods", "method"), ("associated-types", "assoc_type"),
        ("associated-consts", "assoc_const"),
    ];

    let line_lower = line.to_lowercase();
    for (id, kind) in patterns {
        if line_lower.contains(&format!("id=\"{}\"", id)) {
            return Some(kind.to_string());
        }
    }

    let heading_kinds: &[(&str, &str)] = &[
        ("re-exports", "reexport"), ("modules", "mod"), ("structs", "struct"),
        ("enums", "enum"), ("functions", "fn"), ("traits", "trait"),
        ("type aliases", "type"), ("constants", "const"), ("macros", "macro"),
        ("primitive types", "primitive"), ("keywords", "keyword"),
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
            items.push(DocItem {
                path: item_path,
                kind: kind.clone(),
                html_rel: build_html_rel_in_crate(dir, crate_dir_name, kind, name),
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
                items.push(DocItem {
                    path: item_path,
                    kind: kind.clone(),
                    html_rel: build_submodule_html_rel(dir, crate_dir_name, &href),
                    desc: None,
                });
            }
        }
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && read_dir_has_sidebar(&path) {
                let sub_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
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
    let Some(eq_pos) = content.find('=') else { return; };
    let json_str = content[eq_pos + 1..].trim().trim_end_matches(';').trim();
    if let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, Vec<String>>>(json_str) {
        for (kind, names) in map {
            out.insert(kind, names);
        }
    }
}

fn build_html_rel_in_crate(dir: &Path, crate_dir_name: &str, kind: &str, name: &str) -> String {
    format!("{}/{}", path_relative_to_crate_dir(dir, crate_dir_name), html_filename_for_kind(kind, name))
}

fn build_submodule_html_rel(dir: &Path, crate_dir_name: &str, href: &str) -> String {
    format!("{}/{}", path_relative_to_crate_dir(dir, crate_dir_name), href)
}

fn path_relative_to_crate_dir(dir: &Path, crate_dir_name: &str) -> String {
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if dir_name == crate_dir_name {
        crate_dir_name.to_string()
    } else {
        let mut components = Vec::new();
        let mut current = dir;
        loop {
            let name = current.file_name().and_then(|n| n.to_str()).unwrap_or("");
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

    // Normalize h2→h1 and h4→h3 so MarkdownViewer renders them with distinct colors.
    // H2 uses text_color (same as body = grey), so we avoid H2 entirely:
    //   H1 = primary_color (bright blue, bold+underlined)
    //   H3 = secondary_color (warm orange, bold)
    // Also fix bold-wrapped headings (**### ...** → ### ...).
    let preprocessed = main_content
        .replace("<h2", "<h1")
        .replace("</h2>", "</h1>")
        .replace("<h4", "<h3")
        .replace("</h4>", "</h3>");

    let options = html_to_markdown_rs::ConversionOptions::builder()
        .strip_tags(vec!["a".into()])       // keep link text, drop [text](url)
        .exclude_selectors(vec![
            ".anchor".into(),               // rustdoc § anchors
            "#rustdoc-nav".into(),          // sidebar nav
            ".sidebar-links".into(),        // sidebar links
            ".location".into(),             // breadcrumb location
        ])
        .build();

    match html_to_markdown_rs::convert(&preprocessed, Some(options)) {
        Ok(result) => clean_rendered_text(result.content.as_deref().unwrap_or("")),
        Err(e) => format!("Failed to render HTML: {e}"),
    }
}

fn clean_rendered_text(rendered: &str) -> String {
    let lines: Vec<&str> = rendered.lines().collect();
    let mut output = String::new();
    let mut prev_blank = false;
    let mut in_code_block = false;

    for line in &lines {
        // Track fenced code blocks
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
        }

        let text = if in_code_block {
            *line  // preserve indentation inside code blocks
        } else {
            line.trim()
        };

        // Unwrap bold-wrapped headings: **### ...** → ### ...
        let text = if text.starts_with("**#") && text.ends_with("**") && text.contains(" ") {
            &text[2..text.len() - 2]
        } else {
            text
        };

        if text == "Copy item path"
            || text == "Expand description"
            || text == "Run code"
            || (text.starts_with("[](") && text.ends_with(')'))
        {
            continue;
        }
        if text.is_empty() {
            if !prev_blank { output.push('\n'); prev_blank = true; }
        } else {
            if prev_blank && !output.is_empty() { output.push('\n'); }
            output.push_str(text);
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
        .replace('§', "")
}

/// Discover the doc HTML root from a given path.
pub fn discover_doc_root(path: &Path) -> PathBuf {
    if path.join("all.html").exists() {
        return path.to_path_buf();
    }
    if path.join("index.html").exists() && read_dir_has_sidebar(path) {
        return path.to_path_buf();
    }
    let target_doc = path.join("target/doc");
    if target_doc.exists() {
        return target_doc;
    }
    if path.join("Cargo.toml").exists() {
        let td = path.join("target/doc");
        if td.exists() { return td; }
        eprintln!("Hint: run 'cargo doc' in {} first", path.display());
        std::process::exit(1);
    }
    path.to_path_buf()
}
