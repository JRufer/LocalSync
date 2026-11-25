use crate::format_bytes;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use share_smb::{add_or_update_share, model::suggest_share_name, remove_share_by_name};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use sync_core::{
    config::AppConfig,
    db::{EntryRow, FolderStats, GlobalSyncSummary, ShareRow, StateDb, SyncEventRow},
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalState {
    None,
    Searching {
        query: String,
    },
    CreateShare {
        rel_path: String,
        share_name: String,
        read_only: bool,
    },
    ConfirmRemoveShare {
        share_name: String,
        rel_path: String,
    },
}

pub struct TuiApp {
    pub config: AppConfig,
    pub config_path: Option<PathBuf>,
    pub db: StateDb,
    pub tree_state: TreeState<String>,
    pub tree_items: Vec<TreeItem<'static, String>>,
    pub folder_stats: HashMap<String, FolderStats>,
    pub shares: Vec<ShareRow>,
    pub recent_events: Vec<SyncEventRow>,
    pub summary: GlobalSyncSummary,
    pub search_filter: String,
    pub modal: ModalState,
    pub status_banner: String,
    pub skip_samba_commands: bool,
}

impl TuiApp {
    pub fn new(
        config: AppConfig,
        config_path: Option<PathBuf>,
        db: StateDb,
        skip_samba_commands: bool,
    ) -> Result<Self> {
        let mut app = Self {
            config,
            config_path,
            db,
            tree_state: TreeState::default(),
            tree_items: Vec::new(),
            folder_stats: HashMap::new(),
            shares: Vec::new(),
            recent_events: Vec::new(),
            summary: GlobalSyncSummary::default(),
            search_filter: String::new(),
            modal: ModalState::None,
            status_banner: "Ready · Arrows/hjkl: Navigate · Space/Enter: Toggle · s: Share Folder · u: Unshare · /: Filter · r: Refresh · q: Quit".to_string(),
            skip_samba_commands,
        };
        app.refresh_data()?;
        Ok(app)
    }

    pub fn refresh_data(&mut self) -> Result<()> {
        self.folder_stats = self.db.compute_all_folder_stats()?;
        self.shares = self.db.list_shares()?;
        self.recent_events = self.db.list_recent_events(12)?;
        self.summary = self.db.get_global_summary()?;

        let all_entries = self.db.list_all_entries()?;
        let dirs: Vec<EntryRow> = all_entries.into_iter().filter(|e| e.is_dir).collect();
        self.tree_items = build_tree_items(&dirs, &self.folder_stats, &self.search_filter);

        if self.tree_state.selected().is_empty() && !self.tree_items.is_empty() {
            let first_id = self.tree_items[0].identifier().clone();
            self.tree_state.select(vec![first_id]);
        }
        Ok(())
    }

    pub fn selected_rel_path(&self) -> Option<String> {
        self.tree_state.selected().last().cloned()
    }

    pub fn open_share_modal_for_selected(&mut self) {
        let Some(rel_path) = self.selected_rel_path() else {
            self.status_banner = "Select a folder in the tree first.".to_string();
            return;
        };

        let existing_names: Vec<String> = self
            .config
            .shares
            .iter()
            .map(|s| s.share_name.clone())
            .collect();
        let suggested = suggest_share_name(&rel_path, &existing_names);
        self.modal = ModalState::CreateShare {
            rel_path,
            share_name: suggested,
            read_only: false,
        };
    }

    pub fn open_unshare_modal_for_selected(&mut self) {
        let Some(rel_path) = self.selected_rel_path() else {
            return;
        };
        if let Some(stats) = self.folder_stats.get(&rel_path) {
            if let Some(ref share_name) = stats.share_name {
                self.modal = ModalState::ConfirmRemoveShare {
                    share_name: share_name.clone(),
                    rel_path,
                };
                return;
            }
        }
        self.status_banner = format!("Folder '{rel_path}' is not currently shared.");
    }
}

pub fn build_tree_items(
    dirs: &[EntryRow],
    stats_map: &HashMap<String, FolderStats>,
    filter: &str,
) -> Vec<TreeItem<'static, String>> {
    let filter_lower = filter.trim().to_ascii_lowercase();
    let mut children_by_parent: HashMap<Option<String>, Vec<&EntryRow>> = HashMap::new();

    for d in dirs {
        if !filter_lower.is_empty() && !d.rel_path.to_ascii_lowercase().contains(&filter_lower) {
            // Keep ancestors of matching directories visible
            let has_matching_descendant = dirs.iter().any(|cand| {
                cand.rel_path.starts_with(&format!("{}/", d.rel_path))
                    && cand.rel_path.to_ascii_lowercase().contains(&filter_lower)
            });
            if !has_matching_descendant {
                continue;
            }
        }
        children_by_parent
            .entry(d.parent_rel_path.clone())
            .or_default()
            .push(d);
    }

    for list in children_by_parent.values_mut() {
        list.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn build_level(
        parent: Option<String>,
        map: &HashMap<Option<String>, Vec<&EntryRow>>,
        stats_map: &HashMap<String, FolderStats>,
    ) -> Vec<TreeItem<'static, String>> {
        let Some(nodes) = map.get(&parent) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(nodes.len());
        for node in nodes {
            let children = build_level(Some(node.rel_path.clone()), map, stats_map);
            let label = format_node_label(node, stats_map.get(&node.rel_path));
            let item = if children.is_empty() {
                TreeItem::new_leaf(node.rel_path.clone(), label)
            } else {
                TreeItem::new(node.rel_path.clone(), label, children)
                    .expect("unique sibling folder rel_paths")
            };
            out.push(item);
        }
        out
    }

    build_level(None, &children_by_parent, stats_map)
}

fn format_node_label(node: &EntryRow, stats: Option<&FolderStats>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("📁 {}", node.name),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];

    if let Some(st) = stats {
        spans.push(Span::styled(
            format!(
                "  ({} files, {} dirs, {})",
                st.file_count,
                st.subfolder_count,
                format_bytes(st.total_bytes)
            ),
            Style::default().fg(Color::Gray),
        ));

        if let Some(ref sname) = st.share_name {
            let mode = if st.share_read_only.unwrap_or(false) {
                "RO"
            } else {
                "RW"
            };
            spans.push(Span::styled(
                format!("  [SMB3: {sname} ({mode})]"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if st.conflict_count > 0 {
            spans.push(Span::styled(
                format!("  [⚠ {} conflict]", st.conflict_count),
                Style::default().fg(Color::LightRed),
            ));
        }
    }

    Line::from(spans)
}

pub fn run_tui(
    config: AppConfig,
    config_path: Option<PathBuf>,
    db: StateDb,
    skip_samba_commands: bool,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(config, config_path, db, skip_samba_commands)?;
    let res = run_event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw_ui(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match &mut app.modal {
                    ModalState::None => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.tree_state.key_down();
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.tree_state.key_up();
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            app.tree_state.key_right();
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            app.tree_state.key_left();
                        }
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            app.tree_state.toggle_selected();
                        }
                        KeyCode::Char('s') => {
                            app.open_share_modal_for_selected();
                        }
                        KeyCode::Char('u') | KeyCode::Delete => {
                            app.open_unshare_modal_for_selected();
                        }
                        KeyCode::Char('/') => {
                            app.modal = ModalState::Searching {
                                query: app.search_filter.clone(),
                            };
                        }
                        KeyCode::Char('r') => {
                            app.refresh_data()?;
                            app.status_banner = "Refreshed SQLite state and folder tree.".to_string();
                        }
                        _ => {}
                    },
                    ModalState::Searching { query } => match key.code {
                        KeyCode::Esc => {
                            app.modal = ModalState::None;
                        }
                        KeyCode::Enter => {
                            app.search_filter = query.clone();
                            app.modal = ModalState::None;
                            app.refresh_data()?;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                        }
                        _ => {}
                    },
                    ModalState::CreateShare {
                        rel_path,
                        share_name,
                        read_only,
                    } => match key.code {
                        KeyCode::Esc => {
                            app.modal = ModalState::None;
                        }
                        KeyCode::Tab => {
                            *read_only = !*read_only;
                        }
                        KeyCode::Backspace => {
                            share_name.pop();
                        }
                        KeyCode::Char(c) => {
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                share_name.push(c.to_ascii_lowercase());
                            }
                        }
                        KeyCode::Enter => {
                            let rp = rel_path.clone();
                            let sn = share_name.clone();
                            let ro = *read_only;
                            app.modal = ModalState::None;
                            match add_or_update_share(
                                &mut app.config,
                                app.config_path.as_deref(),
                                &app.db,
                                &sn,
                                &rp,
                                ro,
                                None,
                                app.skip_samba_commands,
                            ) {
                                Ok(res) => {
                                    let warn_txt = if res.overlap_warnings.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" (Warning: {})", res.overlap_warnings[0].describe())
                                    };
                                    app.status_banner = format!(
                                        "Shared \\\\{}\\{} -> {}{}",
                                        app.config.tailscale_ip,
                                        res.share.share_name,
                                        res.share.rel_path,
                                        warn_txt
                                    );
                                    app.refresh_data()?;
                                }
                                Err(err) => {
                                    app.status_banner = format!("Error creating share: {err}");
                                }
                            }
                        }
                        _ => {}
                    },
                    ModalState::ConfirmRemoveShare { share_name, .. } => match key.code {
                        KeyCode::Esc | KeyCode::Char('n') => {
                            app.modal = ModalState::None;
                        }
                        KeyCode::Enter | KeyCode::Char('y') => {
                            let sn = share_name.clone();
                            app.modal = ModalState::None;
                            match remove_share_by_name(
                                &mut app.config,
                                app.config_path.as_deref(),
                                &app.db,
                                &sn,
                                app.skip_samba_commands,
                            ) {
                                Ok(_) => {
                                    app.status_banner = format!("Removed SMB3 share [{sn}]");
                                    app.refresh_data()?;
                                }
                                Err(err) => {
                                    app.status_banner = format!("Error removing share: {err}");
                                }
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

fn draw_ui(f: &mut Frame<'_>, app: &mut TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Top Summary Header
    let filter_tag = if app.search_filter.is_empty() {
        String::new()
    } else {
        format!(" · Filter: '{}'", app.search_filter)
    };
    let header_text = format!(
        " LocalSync (CachyOS Drive <-> SMB3) · Tailscale: {} · {} files ({}) · {} dirs · {} shares · {} conflicts{}",
        app.config.tailscale_ip,
        app.summary.total_files,
        format_bytes(app.summary.total_bytes),
        app.summary.total_dirs,
        app.summary.active_shares_count,
        app.summary.conflict_count,
        filter_tag
    );
    let header = Paragraph::new(header_text)
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Overview "),
        );
    f.render_widget(header, chunks[0]);

    // Main Split: Left Folder Tree (65%), Right Inspector & Event Log (35%)
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(63), Constraint::Percentage(37)])
        .split(chunks[1]);

    let tree_widget = Tree::new(&app.tree_items)
        .expect("valid tree items")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Google Drive / Local Root Folder Tree "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(tree_widget, main_chunks[0], &mut app.tree_state);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(main_chunks[1]);

    // Selected Folder Details & Active Shares
    let mut detail_lines = Vec::new();
    if let Some(sel) = app.selected_rel_path() {
        detail_lines.push(Line::from(vec![
            Span::styled("Path: ", Style::default().fg(Color::Gray)),
            Span::styled(
                sel.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        if let Some(st) = app.folder_stats.get(&sel) {
            detail_lines.push(Line::from(format!(
                "Descendant Files: {} ({})",
                st.file_count,
                format_bytes(st.total_bytes)
            )));
            detail_lines.push(Line::from(format!(
                "Descendant Subfolders: {}",
                st.subfolder_count
            )));
            detail_lines.push(Line::from(format!(
                "Pending Sync Items: {} · Conflicts: {}",
                st.pending_count, st.conflict_count
            )));
            if let Some(ref sname) = st.share_name {
                let mode = if st.share_read_only.unwrap_or(false) {
                    "Read-Only"
                } else {
                    "Read/Write"
                };
                detail_lines.push(Line::from(""));
                detail_lines.push(Line::from(Span::styled(
                    format!("Active SMB3 Share: [{sname}] ({mode})"),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                detail_lines.push(Line::from(format!(
                    "Windows/Android: \\\\{}\\{sname}",
                    app.config.tailscale_ip
                )));
                detail_lines.push(Line::from(format!(
                    "Linux/macOS: smb://{}/{sname}",
                    app.config.tailscale_ip
                )));
            } else {
                detail_lines.push(Line::from(""));
                detail_lines.push(Line::from(Span::styled(
                    "Press 's' to export this folder as an SMB3 Tailscale share.",
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
    } else {
        detail_lines.push(Line::from("No folders indexed yet."));
    }

    let details = Paragraph::new(detail_lines)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Folder Inspector & SMB3 Mount URI "),
        );
    f.render_widget(details, right_chunks[0]);

    // Recent Sync Events
    let event_items: Vec<ListItem<'_>> = app
        .recent_events
        .iter()
        .map(|ev| {
            let color = match ev.level.as_str() {
                "error" => Color::Red,
                "conflict" | "warn" => Color::Yellow,
                _ => Color::Gray,
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{}] ", ev.level.to_uppercase()), Style::default().fg(color)),
                Span::raw(ev.message.clone()),
            ]))
        })
        .collect();
    let events_list = List::new(event_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recent Sync & Share Events "),
    );
    f.render_widget(events_list, right_chunks[1]);

    // Bottom Status Bar
    let footer = Paragraph::new(format!(" {}", app.status_banner))
        .style(Style::default().fg(Color::LightCyan))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);

    // Modal overlay
    match &app.modal {
        ModalState::None => {}
        ModalState::Searching { query } => {
            let area = centered_rect(55, 25, f.area());
            f.render_widget(Clear, area);
            let modal = Paragraph::new(vec![
                Line::from("Filter folder tree by name or relative path:"),
                Line::from(""),
                Line::from(Span::styled(
                    format!("> {query}_"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Press Enter to apply filter, Esc to cancel."),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Search Filter "),
            );
            f.render_widget(modal, area);
        }
        ModalState::CreateShare {
            rel_path,
            share_name,
            read_only,
        } => {
            let area = centered_rect(65, 38, f.area());
            f.render_widget(Clear, area);
            let mode_str = if *read_only {
                "Read-Only (yes)"
            } else {
                "Read/Write (no)"
            };
            let modal = Paragraph::new(vec![
                Line::from(format!("Folder: {rel_path}")),
                Line::from(format!(
                    "Local Path: {}",
                    app.config.local_root.join(rel_path).display()
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw("SMB3 Share Name ([a-z0-9-_]): "),
                    Span::styled(
                        format!("{share_name}_"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("Access Mode (Tab to toggle): "),
                    Span::styled(
                        mode_str,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(format!(
                    "Tailscale URI: smb://{}/{}",
                    app.config.tailscale_ip, share_name
                )),
                Line::from("Press Enter to write /etc/samba/shares-syncd.conf & reload smbd, Esc to cancel."),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Create / Update SMB3 Share "),
            );
            f.render_widget(modal, area);
        }
        ModalState::ConfirmRemoveShare {
            share_name,
            rel_path,
        } => {
            let area = centered_rect(55, 25, f.area());
            f.render_widget(Clear, area);
            let modal = Paragraph::new(vec![
                Line::from(format!(
                    "Remove SMB3 share [{share_name}] for folder '{rel_path}'?"
                )),
                Line::from("(Files remain intact on disk and in Google Drive)"),
                Line::from(""),
                Line::from("Press Enter or 'y' to confirm, Esc or 'n' to cancel."),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm Unshare "),
            );
            f.render_widget(modal, area);
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
