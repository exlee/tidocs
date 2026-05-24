use crate::docs::{has_uppercase, kind_badge_to_kinds, match_item_score, DocSource, Registry};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Terminal,
};
use ratatui_markdown::{ThemeConfig, viewer::MarkdownViewer};
use std::io::{self, Stdout};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

/// Main TUI loop
pub fn run(registry: Registry, sources: Vec<DocSource>) {
    // Setup terminal
    enable_raw_mode().expect("failed to enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).expect("failed to enter alt screen");
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    // Install SIGINT handler that restores the terminal before exiting
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    ctrlc::set_handler({
        let running = Arc::clone(&running);
        move || {
            if running.swap(false, std::sync::atomic::Ordering::Relaxed) {
                // First Ctrl-C: restore terminal cleanly and exit
                let _ = disable_raw_mode();
                let mut out = io::stdout();
                let _ = execute!(out, LeaveAlternateScreen);
                let _ = execute!(out, crossterm::cursor::Show);
                std::process::exit(130);
            }
        }
    }).expect("failed to set Ctrl-C handler");

    let result = run_app(&mut terminal, registry, sources);

    // Mark as stopped so a pending SIGINT doesn't double-restore
    running.store(false, std::sync::atomic::Ordering::Relaxed);

    // Restore terminal
    disable_raw_mode().expect("failed to disable raw mode");
    execute!(terminal.backend_mut(), LeaveAlternateScreen).expect("failed to leave alt screen");
    terminal.show_cursor().expect("failed to show cursor");

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
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
    detail_viewer: MarkdownViewer,
    detail_md: String,
    detail_width: usize,
    doc_theme: ThemeConfig,
    /// Send search requests to the worker.
    tx: mpsc::Sender<SearchRequest>,
    /// Receive search results from the worker.
    rx: mpsc::Receiver<SearchReply>,
    /// Monotonically increasing ID for each search request.
    search_id: u64,
}

impl App {
    /// Rebuild the detail viewer if terminal width changed or content changed.
    fn ensure_detail_viewer(&mut self, width: usize) {
        if self.detail_width != width {
            self.detail_viewer = MarkdownViewer::new().with_max_width(width);
            self.detail_viewer.set_content(&self.detail_md, &self.doc_theme);
            self.detail_width = width;
        }
    }
}

fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    registry: Registry,
    sources: Vec<DocSource>,
) -> io::Result<()> {
    let doc_theme = ThemeConfig::builder()
        .with_text_color(Color::Rgb(200, 210, 220))
        .with_muted_text_color(Color::Rgb(120, 140, 160))
        .with_border_color(Color::Rgb(60, 80, 100))
        .with_primary_color(Color::Rgb(130, 200, 255))    // H1: bright blue + bold + underline
        .with_secondary_color(Color::Rgb(255, 180, 100))  // H3: warm orange + bold
        .with_info_color(Color::Rgb(180, 255, 180))       // H2: green-tinted
        .build();

    // Spawn dedicated search thread with (path, kind) tuples
    let all_items: Vec<(String, String)> = registry.all_items()
        .iter()
        .map(|item| (item.path.clone(), item.kind.clone()))
        .collect();
    let (tx_req, rx_req) = mpsc::channel::<SearchRequest>();
    let (tx_rep, rx_rep) = mpsc::channel::<SearchReply>();
    thread::spawn(move || {
        search_worker(all_items, rx_req, tx_rep)
    });

    let mut app = App {
        mode: AppMode::Search,
        registry,
        sources: sources.clone(),
        query: String::new(),
        items: Vec::new(),
        list_state: ListState::default(),
        detail_viewer: MarkdownViewer::new(),
        detail_md: String::new(),
        detail_width: 0,
        doc_theme,
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
                    if key.code == KeyCode::Char('?') {
                        app.mode = AppMode::Help;
                        continue;
                    }
                    if let KeyCode::Char(c) = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'c' | 'g' => break Ok(()),
                                'u' => {
                                    app.query.clear();
                                    submit_search(&mut app);
                                }
                                'w' => {
                                    let trimmed = app.query.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
                                    app.query.truncate(trimmed.len());
                                    submit_search(&mut app);
                                }
                                _ => {}
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
        let (kind_filter, rest_query): (Option<Vec<String>>, String) = if let Some(space_pos) = query.find(' ') {
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
            matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| all_items[a.0].0.cmp(&all_items[b.0].0)));
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
            if app.list_state.selected().is_some_and(|s| s >= app.items.len()) {
                app.list_state.select(if app.items.is_empty() { None } else { Some(0) });
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
                app.detail_width = 0; // force rebuild on next render
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
            let end = app.query
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
            app.detail_viewer.scroll_up(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.detail_viewer.scroll_down(1);
        }
        KeyCode::PageUp | KeyCode::Backspace => {
            app.detail_viewer.page_up();
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.detail_viewer.page_down();
        }
        _ if key.modifiers.contains(KeyModifiers::CONTROL) => match key.code {
            KeyCode::Char('f') | KeyCode::Char('b') => {
                app.detail_viewer.page_down();
            }
            KeyCode::Char('u') => {
                app.detail_viewer.page_up();
            }
            _ => {}
        },
        KeyCode::Home => {
            app.detail_viewer.scroll_to_top();
        }
        KeyCode::End => {
            app.detail_viewer.scroll_to_bottom();
        }
        _ => {}
    }
}

fn navigate_list(app: &mut App, delta: i32) {
    if app.items.is_empty() {
        return;
    }
    let new = match app.list_state.selected() {
        Some(current) => {
            ((current as i32 + delta).clamp(0, (app.items.len() - 1) as i32)) as usize
        }
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
        Constraint::Length(3),  // Search bar
        Constraint::Length(1),  // Status
        Constraint::Min(5),     // Item list
        Constraint::Length(1),  // Footer
    ])
    .split(size);

    // Search bar with blinking cursor block
    let cursor_char = "█";
    let search_text = Text::from(vec![
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(Color::Rgb(180, 80, 80))),
            Span::raw(&app.query),
            Span::styled(cursor_char, Style::default().fg(Color::Rgb(180, 80, 80))),
        ]),
    ]);
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
    let status = Paragraph::new(Line::from(vec![
        Span::raw(format!(
            " {} items (from {} total) - {} sources",
            item_count,
            total,
            app.sources.len(),
        )),
    ]))
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
                    Span::styled(format!(" {} ", kind_str), Style::default().fg(Color::Rgb(30, 30, 30)).bg(kind_color).bold()),
                    Span::styled(format!(" {}", item.path), Style::default().fg(Color::Rgb(30, 30, 30)).bg(Color::Rgb(180, 210, 240))),
                ])
            } else {
                Line::from(vec![
                    Span::styled(format!(" {} ", kind_str), Style::default().fg(kind_color).bold()),
                    Span::styled(format!(" {}", item.path), Style::default().fg(Color::Rgb(200, 210, 220))),
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
            chunks[2].inner(Margin { vertical: 0, horizontal: 1 }),
            &mut scrollbar_state,
        );
    }

    // Footer with key hints
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" enter ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        Span::raw("detail  "),
        Span::styled(" esc ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        Span::raw("quit/clear  "),
        Span::styled(" C-u ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        Span::raw("clear"),
    ]))
    .style(Style::default().fg(Color::Rgb(80, 100, 120)));
    f.render_widget(footer, chunks[3]);
}

fn render_detail(f: &mut Frame, app: &mut App, size: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(3),  // Title bar
        Constraint::Min(5),     // Content
        Constraint::Length(1),  // Footer
    ])
    .split(size);

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

    // Recreate viewer if terminal resized
    let avail_width = chunks[1].width as usize;
    app.ensure_detail_viewer(avail_width);

    // Markdown content
    app.detail_viewer.render(f, chunks[1], &app.doc_theme);

    // Footer with key hints
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" esc/q ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        Span::raw("back  "),
        Span::styled(" j/k/↑/↓ ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        Span::raw("scroll  "),
        Span::styled(" space/backspace ", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
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
        Line::from(vec![
            Span::styled(" 1. Generate docs for your crate:", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "    cargo doc --no-deps        # generates target/doc/your_crate/",
            Style::default().fg(Color::Rgb(200, 210, 220)),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" 2. Point clidoc at the HTML output:", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        ]),
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
        Line::from(vec![
            Span::styled(" Requirements:", Style::default().fg(Color::Rgb(140, 180, 200)).bold()),
        ]),
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

    let help = Paragraph::new(help_lines)
        .block(
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
