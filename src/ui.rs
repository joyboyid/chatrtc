//! Ratatui layout: header / scrollable chat / input bar.

use chrono::{DateTime, Utc};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::message::ServerMessage;

const USERNAME_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
    Color::LightGreen,
    Color::LightCyan,
];

/// One rendered chat line kept in history (survives scroll).
#[derive(Clone)]
pub struct ChatLine {
    pub spans: Vec<Span<'static>>,
}

/// All TUI state lives here — the render loop only reads/draws this struct.
pub struct UiState {
    pub room_name: String,
    pub connection_status: String,
    pub online_users: Vec<String>,
    pub my_username: String,
    pub input: String,
    pub chat_history: Vec<ChatLine>,
    /// Lines scrolled up from the bottom (0 = pinned to latest).
    pub scroll_offset: u16,
    pub should_quit: bool,
    pub cursor_visible: bool,
}

impl UiState {
    pub fn new(username: String) -> Self {
        Self {
            room_name: "General".into(),
            connection_status: "Connecting…".into(),
            online_users: Vec::new(),
            my_username: username,
            input: String::new(),
            chat_history: Vec::new(),
            scroll_offset: 0,
            should_quit: false,
            cursor_visible: true,
        }
    }

    pub fn push_server_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Chat {
                username,
                content,
                timestamp,
            } => {
                self.push_chat(&username, &content, timestamp, false);
            }
            ServerMessage::System { content, timestamp } => {
                self.push_system(&content, timestamp);
            }
            ServerMessage::Private {
                from,
                to,
                content,
                timestamp,
            } => {
                let label = if from == self.my_username {
                    format!("PM → {to}")
                } else {
                    format!("PM ← {from}")
                };
                self.push_chat(&label, &content, timestamp, true);
            }
            ServerMessage::UserList { users } => {
                self.online_users = users;
            }
            ServerMessage::Error { content, timestamp } => {
                self.push_system(&format!("⚠ {content}"), timestamp);
            }
        }
        // New messages pin view to bottom unless user is reading history.
        if self.scroll_offset == 0 {
            self.scroll_offset = 0;
        }
    }

    fn push_chat(
        &mut self,
        username: &str,
        content: &str,
        timestamp: DateTime<Utc>,
        is_private: bool,
    ) {
        let time = timestamp.format("%H:%M:%S").to_string();
        let user_color = username_color(username);
        let prefix = if is_private { "🔒 " } else { "" };

        self.chat_history.push(ChatLine {
            spans: vec![
                Span::styled(
                    format!("{prefix}[{time}] "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{username}: "),
                    Style::default()
                        .fg(user_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(content.to_string()),
            ],
        });
    }

    fn push_system(&mut self, content: &str, timestamp: DateTime<Utc>) {
        let time = timestamp.format("%H:%M:%S").to_string();
        self.chat_history.push(ChatLine {
            spans: vec![
                Span::styled(
                    format!("[{time}] "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    content.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ),
            ],
        });
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                InputAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                InputAction::None
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                if text.is_empty() {
                    return InputAction::None;
                }
                InputAction::Submit(text)
            }
            KeyCode::Esc => {
                self.should_quit = true;
                InputAction::Quit
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                InputAction::None
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                InputAction::None
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                InputAction::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                InputAction::None
            }
            _ => InputAction::None,
        }
    }

    pub fn handle_mouse_scroll(&mut self, row_delta: i16) {
        if row_delta < 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(1);
        } else if row_delta > 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        }
    }
}

pub enum InputAction {
    None,
    Submit(String),
    Quit,
}

/// Draw layout: main area (header/chat/input) + user list sidebar on the right.
pub fn draw(frame: &mut Frame, state: &UiState) {
    let area = frame.area();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(30),    // main chat area
            Constraint::Length(26), // user sidebar
        ])
        .split(area);

    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // chat
            Constraint::Length(3), // input
        ])
        .split(columns[0]);

    draw_header(frame, main[0], state);
    draw_chat(frame, main[1], state);
    draw_input(frame, main[2], state);
    draw_user_list(frame, columns[1], state);
}

fn draw_header(frame: &mut Frame, area: Rect, state: &UiState) {
    let user_count = state.online_users.len();
    let status_color = if state.connection_status == "Connected" {
        Color::Green
    } else {
        Color::Yellow
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", state.room_name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::styled(
            state.connection_status.clone(),
            Style::default().fg(status_color),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!("{user_count} online"),
            Style::default().fg(Color::Cyan),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(" bandit-X ");

    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_chat(frame: &mut Frame, area: Rect, state: &UiState) {
    let lines: Vec<Line> = state
        .chat_history
        .iter()
        .map(|cl| Line::from(cl.spans.clone()))
        .collect();

    let inner_height = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height) as u16;
    // scroll_offset 0 = pinned to bottom; higher = further up in history.
    let scroll = max_scroll.saturating_sub(state.scroll_offset.min(max_scroll));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Messages "),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame, area: Rect, state: &UiState) {
    let prompt = format!("{} › ", state.my_username);
    let line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(username_color(&state.my_username))),
        Span::raw(state.input.clone()),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Input ")
        .border_style(Style::default().fg(Color::Green));

    frame.render_widget(Paragraph::new(line).block(block), area);

    if state.cursor_visible {
        let cursor_x = area.x
            + 1
            + (state.my_username.len() + 3 + state.input.len()) as u16;
        let cursor_y = area.y + 1;
        if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn draw_user_list(frame: &mut Frame, area: Rect, state: &UiState) {
    let count = state.online_users.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(format!(" Members — {count} "));

    if state.online_users.is_empty() {
        let empty = Paragraph::new("No users online")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = state
        .online_users
        .iter()
        .map(|user| {
            let is_me = user == &state.my_username;
            let color = username_color(user);
            let line = if is_me {
                Line::from(vec![
                    Span::styled("● ", Style::default().fg(Color::Green)),
                    Span::styled(
                        user.clone(),
                        Style::default()
                            .fg(color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" (you)", Style::default().fg(Color::DarkGray)),
                ])
            } else {
                Line::from(vec![
                    Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(user.clone(), Style::default().fg(color)),
                ])
            };
            ListItem::new(line)
        })
        .collect();

    frame.render_widget(List::new(items).block(block), area);
}

/// Deterministic color from username hash — same name always same color.
pub fn username_color(name: &str) -> Color {
    let mut hash: u32 = 0;
    for b in name.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as u32);
    }
    USERNAME_COLORS[(hash as usize) % USERNAME_COLORS.len()]
}

/// Poll crossterm events without blocking the async runtime forever.
pub async fn poll_event() -> std::io::Result<Option<Event>> {
    use crossterm::event::{poll, read};
    use std::time::Duration;

    loop {
        if poll(Duration::from_millis(50))? {
            return Ok(Some(read()?));
        }
        tokio::task::yield_now().await;
    }
}

pub fn map_event(event: Event, state: &mut UiState) -> InputAction {
    match event {
        Event::Key(key) => state.handle_key(key),
        Event::Mouse(mouse) => {
            if let MouseEventKind::ScrollUp = mouse.kind {
                state.handle_mouse_scroll(-1);
            } else if let MouseEventKind::ScrollDown = mouse.kind {
                state.handle_mouse_scroll(1);
            }
            InputAction::None
        }
        Event::Resize(_, _) => InputAction::None,
        _ => InputAction::None,
    }
}