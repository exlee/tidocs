use crate::docs::{DocSource, Registry, has_uppercase, kind_badge_to_kinds, match_item_score};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::io::{self, Stdout};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tui_markdown::{Options, StyleSheet, from_str_with_options};

/// Main TUI loop
/// Guard that restores the terminal on Drop (normal exit, panic, or signal).
/// Installed immediately after enabling raw mode + entering alternate screen.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Self {
        enable_raw_mode().expect("failed to enable raw mode");
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).expect("failed to enter alt screen");
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
        let _ = execute!(stdout, Show);
    }
}

pub fn run(registry: Registry, sources: Vec<DocSource>) {
    // Guard ensures terminal is always restored (panic, signal, or normal exit).
    let _guard = TerminalGuard::new();
    let stdout = io::stdout();
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    // Install SIGINT handler — just aborts the app loop; cleanup is handled by the guard.
    ctrlc::set_handler(|| {
        std::process::exit(130);
    })
    .expect("failed to set Ctrl-C handler");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_app(&mut terminal, registry, sources)
    }));

    match result {
        Ok(Err(e)) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        Err(_panic) => {
            eprintln!("Panic occurred");
            std::process::exit(101);
        }
        Ok(Ok(())) => {}
    }
}

enum AppMode {
    Search,
    Detail,
    Help,
}

/// Message sent from the search thread to the main loop.
/// Message sent from main loop to search worker.
struct SearchRequest {
    id: u64,
    query: String,
}

/// Message sent from search worker to main loop.
struct SearchReply {
    id: u64,
    indices: Vec<usize>,
}

struct App {
    mode: AppMode,
    registry: Registry,
    sources: Vec<DocSource>,
    query: String,
    items: Vec<usize>, // indices into registry
    list_state: ListState,
    detail_md: String,
    detail_scroll: u16,
    /// Fragment anchor to scroll to on first render (e.g. "method.add").
    detail_target: Option<String>,
    /// Send search requests to the worker.
    tx: mpsc::Sender<SearchRequest>,
    /// Receive search results from the worker.
    rx: mpsc::Receiver<SearchReply>,
    /// Monotonically increasing ID for each search request.
    search_id: u64,
}

#[derive(Clone)]
struct DocStyleSheet;

impl StyleSheet for DocStyleSheet {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::default()
                .fg(Color::Rgb(130, 200, 255))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default()
                .fg(Color::Rgb(100, 220, 180))
                .add_modifier(Modifier::BOLD),
            3 => Style::default()
                .fg(Color::Rgb(255, 180, 100))
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(Color::Rgb(200, 210, 220))
                .add_modifier(Modifier::BOLD),
        }
    }
    fn code(&self) -> Style {
        Style::default().fg(Color::Rgb(210, 200, 160))
    }
    fn link(&self) -> Style {
        Style::default().fg(Color::Rgb(140, 180, 220))
    }
    fn blockquote(&self) -> Style {
        Style::default().fg(Color::Rgb(120, 140, 160))
    }
    fn heading_meta(&self) -> Style {
        Style::default().fg(Color::Rgb(120, 140, 160))
    }
    fn metadata_block(&self) -> Style {
        Style::default().fg(Color::Rgb(120, 140, 160))
    }
}

/// Foreground color used for code block content to make it visually distinct.
const CODE_FG: Color = Color::Rgb(180, 200, 160);

/// Post-process Text produced by tui-markdown:
/// - Bake Line.base style into spans (ratatui Paragraph ignores Line.style)
/// - Strip "# " heading prefixes (color + modifiers convey level)
/// - Remove ```lang / ``` fence marker lines
/// - Apply CODE_FG foreground and two-space indent to code block lines
fn process_markdown(mut text: Text<'_>) -> Text<'_> {
    let mut in_code_block = false;

    // First pass: mark which lines are inside code blocks (between fence markers).
    let mut code_lines = vec![false; text.lines.len()];
    for (i, line) in text.lines.iter().enumerate() {
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let trimmed = content.trim();
        if trimmed.starts_with("```") && !in_code_block {
            in_code_block = true;
            continue;
        }
        if trimmed == "```" && in_code_block {
            in_code_block = false;
            continue;
        }
        if in_code_block {
            code_lines[i] = true;
        }
    }

    // Second pass: apply fixes per line.
    let mut filtered = Vec::with_capacity(text.lines.len());
    for (i, line) in text.lines.iter_mut().enumerate() {
        // Skip fence marker lines entirely.
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let trimmed = content.trim();
        if trimmed.starts_with("```") {
            continue;
        }

        // Bake base style into spans so ratatui actually paints them.
        let base = line.style;
        if base != Style::default() {
            for span in &mut line.spans {
                span.style = base.patch(span.style);
            }
        }

        // Strip leading "#" prefix (one or more hashes + space) from heading lines.
        if line.style.fg.is_some() && line.style.add_modifier.contains(Modifier::BOLD) {
            if let Some(first_span) = line.spans.first_mut() {
                *first_span.content.to_mut() = first_span
                    .content
                    .as_ref()
                    .trim_start_matches('#')
                    .trim_start_matches(' ')
                    .to_string();
            }
        }

        // Apply code styling: two-space indent + distinct foreground.
        if code_lines[i] {
            // Prepend two spaces to the first span.
            if let Some(first_span) = line.spans.first_mut() {
                *first_span.content.to_mut() = format!("  {}", first_span.content.as_ref());
            }
            // Override fg on every span with CODE_FG.
            for span in &mut line.spans {
                span.style.fg = Some(CODE_FG);
            }
        }

        filtered.push(std::mem::take(line));
    }

    text.lines = filtered;
    text
}

impl App {
    fn detail_text(&self) -> Text<'_> {
        let options = Options::new(DocStyleSheet);
        let raw = from_str_with_options(&self.detail_md, &options);
        process_markdown(raw)
    }
}

/// Find the line index of a heading whose content matches `target_name`.
/// Headings are identified by having a colored fg + bold modifier on the first span.
fn find_heading_line(text: &[Line<'_>], target_name: &str) -> Option<usize> {
    let lower_target = target_name.to_lowercase();
    for (i, line) in text.iter().enumerate() {
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if let Some(first_span) = line.spans.first() {
            if first_span.style.fg.is_some()
                && first_span.style.add_modifier.contains(Modifier::BOLD)
            {
                if content.to_lowercase().contains(&lower_target) {
                    return Some(i);
                }
            }
        }
    }
    None
}

fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    registry: Registry,
    sources: Vec<DocSource>,
) -> io::Result<()> {
    // Spawn dedicated search thread with (path, kind) tuples
    let all_items: Vec<(String, String)> = registry
        .all_items()
        .iter()
        .map(|item| (item.path.clone(), item.kind.clone()))
        .collect();
    let (tx_req, rx_req) = mpsc::channel::<SearchRequest>();
    let (tx_rep, rx_rep) = mpsc::channel::<SearchReply>();
    thread::spawn(move || search_worker(all_items, rx_req, tx_rep));

    let mut app = App {
        mode: AppMode::Search,
        registry,
        sources: sources.clone(),
        query: String::new(),
        items: Vec::new(),
        list_state: ListState::default(),
        detail_md: String::new(),
        detail_scroll: 0,
        detail_target: None,
        tx: tx_req,
        rx: rx_rep,
        search_id: 0,
    };

    loop {
        // Drain any completed search results before drawing
        drain_results(&mut app);

        terminal.draw(|f| render(f, &mut app))?;

        if crossterm::event::poll(Duration::from_millis(33))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.mode {
                AppMode::Search => {
                    if let KeyCode::Char(c) = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'c' | 'g' => break Ok(()),
                                'u' => {
                                    app.query.clear();
                                    submit_search(&mut app);
                                }
                                'w' => {
                                    let trimmed = app.query.trim_end_matches(|c: char| {
                                        c.is_alphanumeric() || c == '_'
                                    });
                                    app.query.truncate(trimmed.len());
                                    submit_search(&mut app);
                                }
                                _ => { 
                                    handle_search_key(&mut app, key);
                                },
                            }
                        } else {
                            app.query.push(c);
                            submit_search(&mut app);
                        }
                    } else if handle_search_key(&mut app, key) {
                        break Ok(());
                    }
                }
                AppMode::Detail => handle_detail_key(&mut app, key),
                AppMode::Help => {
                    app.mode = AppMode::Search;
                }
            }
        }
    }
}

/// Dedicated background thread that runs substring searches.
fn search_worker(
    all_items: Vec<(String, String)>, // (path, kind)
    rx_req: mpsc::Receiver<SearchRequest>,
    tx_rep: mpsc::Sender<SearchReply>,
) {
    while let Ok(SearchRequest { id, query }) = rx_req.recv() {
        // Check for leading kind badge
        let (kind_filter, rest_query): (Option<Vec<String>>, String) =
            if let Some(space_pos) = query.find(' ') {
                let badge = &query[..space_pos];
                let kinds = kind_badge_to_kinds(badge);
                if !kinds.is_empty() {
                    (Some(kinds), query[space_pos + 1..].to_string())
                } else {
                    (None, query.clone())
                }
            } else {
                (None, query.clone())
            };
        let words: Vec<&str> = rest_query.split_whitespace().collect();
        let case_sensitive = has_uppercase(&rest_query);
        let indices = if words.is_empty() {
            Vec::new()
        } else {
            let mut matches: Vec<(usize, i32)> = Vec::new();
            for (i, (path, kind)) in all_items.iter().enumerate() {
                if let Some(ref kf) = kind_filter {
                    if !kf.contains(kind) {
                        continue;
                    }
                }
                if let Some(score) = match_item_score(path, kind, &words, case_sensitive) {
                    matches.push((i, score));
                }
            }
            matches.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| all_items[a.0].0.cmp(&all_items[b.0].0))
            });
            matches.into_iter().map(|(i, _)| i).collect()
        };
        let _ = tx_rep.send(SearchReply { id, indices });
    }
}

/// Submit a new async search request.
fn submit_search(app: &mut App) {
    app.search_id += 1;
    let id = app.search_id;
    let query = app.query.clone();
    let _ = app.tx.send(SearchRequest { id, query });
}

/// Drain completed search results from the channel and apply the latest one.
fn drain_results(app: &mut App) {
    // Collect all pending replies
    let mut pending: Vec<SearchReply> = Vec::new();
    loop {
        match app.rx.try_recv() {
            Ok(reply) => pending.push(reply),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => return,
        }
    }
    // Apply only the reply matching our current search_id
    for reply in pending {
        if reply.id == app.search_id {
            app.items = reply.indices;
            if app
                .list_state
                .selected()
                .is_some_and(|s| s >= app.items.len())
            {
                app.list_state
                    .select(if app.items.is_empty() { None } else { Some(0) });
            }
            if let Some(selected) = app.list_state.selected() {
                prefetch_around(app, selected);
            }
        }
    }
}

/// Handle a key press in search mode. Returns true if the app should quit.
fn handle_search_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        _ if key.modifiers.contains(KeyModifiers::CONTROL) => match key.code {
            KeyCode::Char('f') => navigate_list(app, 15),
            KeyCode::Char('b') => navigate_list(app, -15),
            KeyCode::Char('n') => navigate_list(app, 1),
            KeyCode::Char('p') => {
                if app.list_state.selected().is_some() {
                    navigate_list(app, -1);
                }
            }
            _ => {}
        },
        KeyCode::Esc => {
            if app.query.is_empty() {
                return true;
            }
            app.query.clear();
            submit_search(app);
        }
        KeyCode::Enter => {
            if let Some(selected) = app.list_state.selected()
                && let Some(&idx) = app.items.get(selected)
            {
                let item = &app.registry.all_items()[idx];
                app.detail_md = app.registry.load_doc_content(&item.html_rel);
                app.detail_scroll = 0;
                // Extract fragment anchor from html_rel (e.g. "#method.add" -> "add")
                app.detail_target = item
                    .html_rel
                    .split('#')
                    .nth(1)
                    .map(|frag| frag.split('.').last().unwrap_or(frag).to_string());
                app.mode = AppMode::Detail;
            }
        }
        KeyCode::Up => {
            if app.list_state.selected().is_some() {
                navigate_list(app, -1);
            }
        }
        KeyCode::Down => navigate_list(app, 1),
        KeyCode::PageUp => navigate_list(app, -15),
        KeyCode::PageDown => navigate_list(app, 15),
        KeyCode::Home => {
            if !app.items.is_empty() {
                app.list_state.select(Some(0));
            }
        }
        KeyCode::End => {
            if !app.items.is_empty() {
                app.list_state.select(Some(app.items.len() - 1));
            }
        }
        KeyCode::Backspace => {
            app.query.pop();
            submit_search(app);
        }
        KeyCode::Delete => {
            let end = app
                .query
                .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_')
                .len();
            app.query.truncate(end);
            submit_search(app);
        }
        _ => {}
    }
    false
}

fn handle_detail_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Search;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.detail_scroll = app.detail_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.detail_scroll = app.detail_scroll.saturating_add(1);
        }
        KeyCode::PageUp | KeyCode::Backspace => {
            app.detail_scroll = app.detail_scroll.saturating_sub(15);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.detail_scroll = app.detail_scroll.saturating_add(15);
        }
        _ if key.modifiers.contains(KeyModifiers::CONTROL) => match key.code {
            KeyCode::Char('f') => {
                app.detail_scroll = app.detail_scroll.saturating_add(15);
            }
            KeyCode::Char('b') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(15);
            }
            KeyCode::Char('u') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(15);
            }
            _ => {}
        },
        KeyCode::Home => {
            app.detail_scroll = 0;
        }
        KeyCode::End => {
            app.detail_scroll = u16::MAX;
        }
        _ => {}
    }
}

fn navigate_list(app: &mut App, delta: i32) {
    if app.items.is_empty() {
        return;
    }
    let new = match app.list_state.selected() {
        Some(current) => ((current as i32 + delta).clamp(0, (app.items.len() - 1) as i32)) as usize,
        None => {
            // No item selected yet: Down picks first, Up picks last
            if delta > 0 { 0 } else { app.items.len() - 1 }
        }
    };
    app.list_state.select(Some(new));
    prefetch_around(app, new);
}

fn prefetch_around(app: &App, center: usize) {
    let all = app.registry.all_items();
    let mut rels = Vec::new();
    for &offset in &[-2i32, -1, 0, 1, 2] {
        let idx = (center as i32 + offset).clamp(0, app.items.len() as i32 - 1) as usize;
        if let Some(&item_idx) = app.items.get(idx) {
            let rel = all[item_idx].html_rel.clone();
            if !rels.contains(&rel) {
                rels.push(rel);
            }
        }
    }
    app.registry.prefetch(rels);
}

fn render(f: &mut Frame, app: &mut App) {
    let size = f.area();
    f.render_widget(Clear, size);

    match app.mode {
        AppMode::Search => render_search(f, app, size),
        AppMode::Detail => render_detail(f, app, size),
        AppMode::Help => render_help(f, size),
    }
}

fn render_search(f: &mut Frame, app: &mut App, size: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // Search bar
        Constraint::Length(1), // Status
        Constraint::Min(5),    // Item list
        Constraint::Length(1), // Footer
    ])
    .split(size);

    // Clear each region independently to avoid artifacts from previous frame layouts.
    for chunk in chunks.as_ref() {
        f.render_widget(Clear, *chunk);
    }

    // Search bar with blinking cursor block
    let cursor_char = "█";
    let search_text = Text::from(vec![Line::from(vec![
        Span::styled(" / ", Style::default().fg(Color::Rgb(180, 80, 80))),
        Span::raw(&app.query),
        Span::styled(cursor_char, Style::default().fg(Color::Rgb(180, 80, 80))),
    ])]);
    let search = Paragraph::new(search_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(100, 120, 140)))
                .title(" clidoc "),
        )
        .style(Style::default().fg(Color::White));
    f.render_widget(search, chunks[0]);

    // Status line
    let item_count = app.items.len();
    let total = app.registry.all_items().len();
    let status = Paragraph::new(Line::from(vec![Span::raw(format!(
        " {} items (from {} total) - {} sources",
        item_count,
        total,
        app.sources.len(),
    ))]))
    .style(Style::default().fg(Color::Rgb(120, 140, 160)));
    f.render_widget(status, chunks[1]);

    // Item list
    let list_items: Vec<ListItem> = app
        .items
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let item = &app.registry.all_items()[idx];
            let selected = app.list_state.selected() == Some(i);
            let (kind_color, kind_str) = kind_display(&item.kind);

            let line = if selected {
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", kind_str),
                        Style::default()
                            .fg(Color::Rgb(30, 30, 30))
                            .bg(kind_color)
                            .bold(),
                    ),
                    Span::styled(
                        format!(" {}", item.path),
                        Style::default()
                            .fg(Color::Rgb(30, 30, 30))
                            .bg(Color::Rgb(180, 210, 240)),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", kind_str),
                        Style::default().fg(kind_color).bold(),
                    ),
                    Span::styled(
                        format!(" {}", item.path),
                        Style::default().fg(Color::Rgb(200, 210, 220)),
                    ),
                ])
            };

            ListItem::new(line)
        })
        .collect();

    let list = List::new(list_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(80, 100, 120))),
    );
    f.render_stateful_widget(list, chunks[2], &mut app.list_state);

    // Scrollbar for list
    if !app.items.is_empty() {
        let max_scroll = app.items.len().saturating_sub(chunks[2].height as usize);
        let selected = app.list_state.selected().unwrap_or(0);
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(selected);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(Color::Rgb(80, 100, 120)))
                .begin_symbol(Some("\u{25b2}"))
                .end_symbol(Some("\u{25bc}")),
            chunks[2].inner(Margin {
                vertical: 0,
                horizontal: 1,
            }),
            &mut scrollbar_state,
        );
    }

    // Footer row: split into left (key hints) and right (tip of the day)
    let footer_chunks = Layout::horizontal([
        Constraint::Percentage(65),
        Constraint::Percentage(35),
    ])
    .split(chunks[3]);

    let keys = Line::from(vec![
        Span::styled(
            " enter ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("detail  "),
        Span::styled(
            "C-n/up ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("next  "),
        Span::styled(
            "C-p/down ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("prev  "),
        Span::styled(
            "C-f/b ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("page  "),
        Span::styled(
            " esc ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("quit  "),
        Span::styled(
            "C-u ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("clear"),
    ]);
    let tip = Line::raw("cargo doc -p <PKG> && clidoc target/doc");
    let footer = Paragraph::new(keys).style(Style::default().fg(Color::Rgb(80, 100, 120)));
    let tip_para = Paragraph::new(tip)
        .style(Style::default().fg(Color::Rgb(80, 100, 120)))
        .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(footer, footer_chunks[0]);
    f.render_widget(tip_para, footer_chunks[1]);
}

fn render_detail(f: &mut Frame, app: &mut App, size: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // Title bar
        Constraint::Min(5),    // Content
        Constraint::Length(1), // Footer
    ])
    .split(size);

    // Clear each region independently to avoid artifacts from previous frame layouts.
    for chunk in chunks.as_ref() {
        f.render_widget(Clear, *chunk);
    }

    // Title bar
    let title = if let Some(selected) = app.list_state.selected() {
        if let Some(&idx) = app.items.get(selected) {
            let item = &app.registry.all_items()[idx];
            item.path.clone()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let title_bar = Paragraph::new(Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(title, Style::default().fg(Color::Rgb(200, 220, 240)).bold()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(80, 120, 160)))
            .title(" doc "),
    );
    f.render_widget(title_bar, chunks[0]);

    // Markdown content
    let visible = chunks[1].height.saturating_sub(2) as usize;
    let target = app.detail_target.take();
    let text = app.detail_text();

    // If there's a pending fragment target, find matching heading and adjust scroll.
    if let Some(tgt) = target {
        if let Some(line_idx) = find_heading_line(&text.lines, &tgt) {
            let max_s = text.lines.len().saturating_sub(visible);
            let new_scroll = (line_idx.saturating_sub(2)).min(max_s);
            drop(text);
            app.detail_scroll = new_scroll as u16;
        }
    }

    // Re-render with (possibly updated) scroll position.
    let text = app.detail_text();
    let line_count = text.lines.len();
    let max_scroll = line_count.saturating_sub(visible) as u16;
    let scroll = app.detail_scroll.min(max_scroll);

    let content = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(60, 70, 80))),
        )
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(content, chunks[1]);

    // Footer with key hints
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " esc/q ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("back  "),
        Span::styled(
            " j/k/↑/↓ ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("scroll  "),
        Span::styled(
            " space/backspace ",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        ),
        Span::raw("page"),
    ]))
    .style(Style::default().fg(Color::Rgb(80, 100, 120)));
    f.render_widget(footer, chunks[2]);
}

fn render_help(f: &mut Frame, size: Rect) {
    let help_lines = vec![
        Line::from(Span::styled(
            " How to add documentation sources",
            Style::default().fg(Color::Rgb(200, 220, 240)).bold(),
        )),
        Line::raw(""),
        Line::from(vec![Span::styled(
            " 1. Generate docs for your crate:",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        )]),
        Line::raw(""),
        Line::from(Span::styled(
            "    cargo doc --no-deps        # generates target/doc/your_crate/",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::raw(""),
        Line::from(vec![Span::styled(
            " 2. Point clidoc at the HTML output:",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        )]),
        Line::raw(""),
        Line::from(Span::styled(
            "    clidoc ./target/doc          # multi-crate root with all.html",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::from(Span::styled(
            "    clidoc ./target/doc/your_crate   # single crate dir",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::from(Span::styled(
            "    clidoc                         # default: rustup std docs",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::raw(""),
        Line::from(vec![Span::styled(
            " Requirements:",
            Style::default().fg(Color::Rgb(140, 180, 200)).bold(),
        )]),
        Line::raw(""),
        Line::from(Span::styled(
            "    The directory must contain 'all.html' or 'sidebar-items*.js'",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::from(Span::styled(
            "    (standard output of 'cargo doc')",
            Style::default().fg(Color::Rgb(140, 160, 180)),
        )),
        Line::raw(""),
        Line::styled(
            " Press any key to close",
            Style::default().fg(Color::Rgb(100, 120, 140)),
        ),
    ];

    let help = Paragraph::new(help_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(80, 120, 160)))
            .title(" How to add docs "),
    );

    // Center the help box
    let popup_width = 56.min(size.width.saturating_sub(4));
    let popup_height = 15.min(size.height.saturating_sub(4));
    let x = (size.width.saturating_sub(popup_width)) / 2;
    let y = (size.height.saturating_sub(popup_height)) / 2;
    let area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, area);
    f.render_widget(help, area);
}

fn kind_display(kind: &str) -> (Color, &'static str) {
    match kind {
        "fn" | "method" => (Color::Rgb(86, 182, 194), "fn"),
        "trait" => (Color::Rgb(180, 140, 220), "tr"),
        "struct" => (Color::Rgb(130, 180, 120), "st"),
        "enum" => (Color::Rgb(200, 160, 100), "en"),
        "mod" => (Color::Rgb(120, 150, 200), "md"),
        "macro" => (Color::Rgb(200, 120, 120), "ma"),
        "type" | "assoc_type" => (Color::Rgb(160, 180, 140), "ty"),
        "const" | "constant" | "assoc_const" => (Color::Rgb(200, 200, 130), "co"),
        "primitive" => (Color::Rgb(140, 160, 180), "pr"),
        "keyword" => (Color::Rgb(180, 140, 140), "kw"),
        "reexport" => (Color::Rgb(150, 150, 150), ">>"),
        _ => (Color::Rgb(150, 150, 150), "??"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui_markdown::{Options, from_str_with_options};

    /// Render markdown through our full pipeline (DocStyleSheet + post-processing).
    fn render(md: &str) -> Text<'_> {
        let options = Options::new(DocStyleSheet);
        let raw = from_str_with_options(md, &options);
        super::process_markdown(raw)
    }

    /// Format each line as `"<content> [styles...]"` for readable assertion output.
    /// Describe a single Style as tokens like `fg=Rgb(130,200,255) bold underlined`, or `-` for default.
    fn describe_style(style: &Style) -> String {
        let mut parts = Vec::new();
        match style.fg {
            Some(c) => parts.push(format!("fg={:?}", c)),
            None => {}
        }
        match style.bg {
            Some(c) => parts.push(format!("bg={:?}", c)),
            None => {}
        }
        let mods = style.add_modifier;
        if mods.contains(Modifier::BOLD) {
            parts.push("bold".into());
        }
        if mods.contains(Modifier::ITALIC) {
            parts.push("italic".into());
        }
        if mods.contains(Modifier::UNDERLINED) {
            parts.push("underlined".into());
        }
        if mods.contains(Modifier::DIM) {
            parts.push("dim".into());
        }
        if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join(" ")
        }
    }

    /// Format each rendered line as:
    ///   `<idx>: <content> |base=<line-level-style>| <span-styles...>`
    /// The `base=` column shows the ratatui Line's patched base style (where heading colors land).
    /// Span styles are shown per-span; `-` means that span has no extra styling beyond the base.
    fn format_text(text: &Text) -> String {
        text.lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let base = describe_style(&line.style);
                let spans: Vec<String> = line
                    .spans
                    .iter()
                    .map(|s| describe_style(&s.style))
                    .collect();
                format!("{:04}: {:?} |{}| {}", i, content, base, spans.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full rustdoc-like page: heading hierarchy, paragraphs, lists, code blocks, inline code.
    #[test]
    fn full_rustdoc_page_snapshot() {
        let md = "# Peek<T>(struct)\n\nPeek at the next item in an iterator without consuming it.\n\n### Implementations\n\n#### `impl<T, I> Peek<T, I>`\n\nMethods on `Peek` that take `self`.\n\n##### `pub fn peek(&mut self) -> Option<&T>`\n\nLooks at the second element of an iterator **without advancing it**.\n\n```rust\nlet mut iter = vec![1, 2, 3].into_iter().peekable();\nassert_eq!(iter.peek(), Some(&1));\n```\n\n##### `pub fn peek_mut(&mut self) -> Option<&mut T>`\n\nLike [peek] but returns a mutable reference.\n\n- Returns `None` when the iterator is exhausted\n- Does not consume the element\n\n###### `fn example()`\n\nThis is a deeply nested heading.";
        let text = render(md);
        let formatted = format_text(&text);
        insta::assert_snapshot!("full_rustdoc_page", formatted);
    }

    /// After post-processing, heading colors live on Span.style (baked from Line.base).
    #[test]
    fn h1_is_bright_blue_bold_underlined() {
        let text = render("# Main Title");
        let line = &text.lines[0];
        // Content should no longer have the "# " prefix.
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            content, "Main Title",
            "heading prefix not stripped: {:?}",
            content
        );
        // First non-empty span carries the H1 color.
        let span = line.spans.first().expect("empty line");
        assert_eq!(span.style.fg, Some(Color::Rgb(130, 200, 255)));
        assert!(
            span.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
    }

    #[test]
    fn h2_is_teal_bold() {
        let text = render("## Subtitle");
        let line = &text.lines[0];
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "Subtitle");
        let span = line.spans.first().expect("empty line");
        assert_eq!(span.style.fg, Some(Color::Rgb(100, 220, 180)));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(!span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn h3_is_orange_bold() {
        let text = render("### Section");
        let line = &text.lines[0];
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "Section");
        let span = line.spans.first().expect("empty line");
        assert_eq!(span.style.fg, Some(Color::Rgb(255, 180, 100)));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_block_styled_indented_no_fences() {
        let md = r#"
```rust
fn main() {
    println!("hello");
        deeply_nested();
}
```
"#;
        let text = render(md);
        let formatted = format_text(&text);
        insta::assert_snapshot!("code_fence_indentation", formatted);

        let lines_str: Vec<String> = text.lines.iter().map(|l| l.to_string()).collect();

        // Fence markers should be stripped.
        assert!(
            !lines_str.iter().any(|l| l.starts_with("```")),
            "fence markers should be removed: {:?}",
            lines_str
        );

        // Two-space indent added + original indentation preserved.
        let println_line = lines_str
            .iter()
            .find(|l| l.contains("println"))
            .unwrap_or_else(|| panic!("no println line in {:?}", lines_str));
        assert!(
            println_line.starts_with("      "),
            "2+4 space indent expected: {:?}",
            println_line
        );

        let nested_line = lines_str
            .iter()
            .find(|l| l.contains("deeply_nested"))
            .unwrap_or_else(|| panic!("no deeply_nested line in {:?}", lines_str));
        assert!(
            nested_line.starts_with("          "),
            "2+8 space indent expected: {:?}",
            nested_line
        );

        // Code content spans carry CODE_FG foreground.
        for line in &text.lines {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if content.trim().is_empty() {
                continue;
            }
            for span in &line.spans {
                assert_eq!(
                    span.style.fg,
                    Some(CODE_FG),
                    "code content should have CODE_FG fg on {:?}",
                    content
                );
            }
        }
    }
}
