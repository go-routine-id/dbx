//! Screen S1: Connection Picker & Form Modal P5.
//! Manages saved connections list and add/edit forms with keyboard-first navigation.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::config::{ConnectionConfig, DriverType};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormField {
    Name,
    Driver,
    Host,
    Port,
    User,
    Password,
    Database,
}

impl FormField {
    pub const ALL: [FormField; 7] = [
        FormField::Name,
        FormField::Driver,
        FormField::Host,
        FormField::Port,
        FormField::User,
        FormField::Password,
        FormField::Database,
    ];

    pub fn next(&self) -> Self {
        match self {
            FormField::Name => FormField::Driver,
            FormField::Driver => FormField::Host,
            FormField::Host => FormField::Port,
            FormField::Port => FormField::User,
            FormField::User => FormField::Password,
            FormField::Password => FormField::Database,
            FormField::Database => FormField::Name,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            FormField::Name => FormField::Database,
            FormField::Driver => FormField::Name,
            FormField::Host => FormField::Driver,
            FormField::Port => FormField::Host,
            FormField::User => FormField::Port,
            FormField::Password => FormField::User,
            FormField::Database => FormField::Password,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            FormField::Name => "Name",
            FormField::Driver => "Driver",
            FormField::Host => "Host",
            FormField::Port => "Port",
            FormField::User => "Username",
            FormField::Password => "Password",
            FormField::Database => "Database",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionForm {
    pub is_editing: bool,
    pub original_name: Option<String>,
    pub focused_field: FormField,
    pub name: String,
    pub driver: DriverType,
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
    pub database: String,
    /// TLS settings, preserved through edit so saving never downgrades them.
    pub ssl: bool,
    pub ssl_mode: Option<crate::config::SslMode>,
    /// SSH tunnel settings. Not edited in the form (config file only), but
    /// preserved through edit so saving never silently drops the tunnel.
    pub ssh: Option<crate::config::SshConfig>,
    /// Last Ctrl+T result, kept visible inside the modal so the user always
    /// sees the test outcome right where they're editing. Cleared on the next
    /// Ctrl+T press.
    pub last_test_result: Option<crate::ui::screens::picker::TestResult>,
}

#[derive(Clone, Debug)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
}

/// State for the destructive-action confirmation dialog shown when the user
/// presses `d` on a saved connection. Stores the original index so the
/// delete can run even if the picker selection has moved in the meantime.
#[derive(Clone, Debug)]
pub struct ConfirmDeleteModal {
    pub connection_name: String,
    pub connection_index: usize,
}

impl ConnectionForm {
    pub fn new_empty() -> Self {
        // Environment-provided defaults so the form isn't hardcoded to a
        // specific machine. All of these are optional; a connection is still
        // valid with empty fields (host/port fall back to driver defaults
        // when building the `ConnectionConfig`).
        let env_default = |key: &str| std::env::var(key).unwrap_or_default();
        let env_port = env_default("DBX_DEFAULT_PORT");
        Self {
            is_editing: false,
            original_name: None,
            focused_field: FormField::Name,
            name: String::new(),
            driver: DriverType::MySql,
            host: env_default("DBX_DEFAULT_HOST"),
            port: if env_port.is_empty() {
                DriverType::MySql.default_port().to_string()
            } else {
                env_port
            },
            user: env_default("DBX_DEFAULT_USER"),
            password: env_default("DBX_DEFAULT_PASSWORD"),
            database: env_default("DBX_DEFAULT_DATABASE"),
            ssl: false,
            ssl_mode: None,
            ssh: None,
            last_test_result: None,
        }
    }

    pub fn from_config(cfg: &ConnectionConfig) -> Self {
        Self {
            is_editing: true,
            original_name: Some(cfg.name.clone()),
            focused_field: FormField::Name,
            name: cfg.name.clone(),
            driver: cfg.driver.clone(),
            host: cfg.host.clone(),
            port: cfg.port.map(|p| p.to_string()).unwrap_or_else(|| cfg.driver.default_port().to_string()),
            user: cfg.user.clone().unwrap_or_default(),
            password: cfg.password.clone().unwrap_or_default(),
            database: cfg.database.clone().unwrap_or_default(),
            ssl: cfg.ssl,
            ssl_mode: cfg.ssl_mode,
            ssh: cfg.ssh.clone(),
            last_test_result: None,
        }
    }

    pub fn to_connection_config(&self) -> ConnectionConfig {
        ConnectionConfig {
            name: self.name.trim().to_string(),
            driver: self.driver.clone(),
            host: if self.host.trim().is_empty() { crate::config::DEFAULT_HOST.to_string() } else { self.host.trim().to_string() },
            port: self.port.trim().parse::<u16>().ok(),
            user: if self.user.trim().is_empty() { None } else { Some(self.user.trim().to_string()) },
            password: if self.password.is_empty() { None } else { Some(self.password.clone()) },
            database: if self.database.trim().is_empty() { None } else { Some(self.database.trim().to_string()) },
            socket: None,
            ssl: self.ssl,
            ssl_mode: self.ssl_mode,
            ssh: self.ssh.clone(),
        }
    }
}

pub fn render_picker(
    f: &mut Frame,
    area: Rect,
    connections: &[ConnectionConfig],
    selected_index: usize,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border())
        .style(theme.base())
        .title(" Saved Connections ");

    if connections.is_empty() {
        let text = vec![
            Line::from(Span::styled("No saved connections found.", theme.dim())),
            Line::from(Span::styled("Press 'a' to add your first database connection.", theme.accent())),
        ];
        let p = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = connections
        .iter()
        .enumerate()
        .map(|(i, conn)| {
            let is_sel = i == selected_index;
            let marker = if is_sel { "▶ " } else { "  " };
            let name_style = if is_sel {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.base().add_modifier(Modifier::BOLD)
            };

            let line = Line::from(vec![
                Span::styled(marker, theme.accent()),
                Span::styled(format!("{:<20}", conn.name), name_style),
                Span::styled("  ", theme.dim()),
                Span::styled(conn.display_url(), theme.dim()),
            ]);

            ListItem::new(line)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(selected_index));

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.selected());

    f.render_stateful_widget(list, area, &mut state);
}

pub fn render_form_modal(
    f: &mut Frame,
    area: Rect,
    form: &ConnectionForm,
    is_testing: bool,
    spinner: &crate::ui::widgets::spinner::Spinner,
    theme: &Theme,
) {
    let popup_width = 64.min(area.width.saturating_sub(4));
    let popup_height = 20.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);

    let title = if form.is_editing { " Edit Connection (P5) " } else { " New Connection (P5) " };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(title);

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2), // Name
            Constraint::Length(2), // Driver
            Constraint::Length(2), // Host
            Constraint::Length(2), // Port
            Constraint::Length(2), // User
            Constraint::Length(2), // Password
            Constraint::Length(2), // Database
            Constraint::Length(1), // Test status (spinner) — only when is_testing
            Constraint::Length(1), // Help footer
        ])
        .split(inner);

    let render_input = |f: &mut Frame, r: Rect, field: FormField, val: &str, is_secret: bool| {
        let is_focused = form.focused_field == field;
        let label_style = if is_focused {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };

        // SQLite has no server: `database` carries the file path, so the
        // label says so rather than the generic "Database".
        let label_text = if field == FormField::Database && form.driver == DriverType::Sqlite {
            "File path"
        } else if field == FormField::Database && form.driver == DriverType::Redis {
            // redis: `database` carries the numeric logical-db index.
            "DB index"
        } else {
            field.label()
        };
        let label_str = format!("{label_text:<10} : ");
        let mut spans = vec![Span::styled(label_str, label_style)];

        if field == FormField::Driver {
            let driver_str = match form.driver {
                DriverType::MySql => "MySQL",
                DriverType::Postgres => "PostgreSQL",
                DriverType::SqlServer => "SQL Server",
                DriverType::Sqlite => "SQLite",
                DriverType::Redis => "Redis", // redis
            };
            if is_focused {
                spans.push(Span::styled("◀ ", theme.accent()));
                spans.push(Span::styled(driver_str, theme.accent().add_modifier(Modifier::BOLD)));
                spans.push(Span::styled(" ▶ (press Space/←/→ to change)", theme.dim()));
            } else {
                spans.push(Span::styled(driver_str, theme.base()));
            }
        } else {
            let display_text = if is_secret && !val.is_empty() {
                if val.starts_with("$ENV:") {
                    val.to_string()
                } else {
                    "•".repeat(val.len())
                }
            } else {
                val.to_string()
            };

            if is_focused {
                spans.push(Span::styled(display_text, theme.base().add_modifier(Modifier::BOLD)));
                spans.push(Span::styled("█", theme.accent()));
            } else if val.is_empty() {
                spans.push(Span::styled("(empty)", theme.dim()));
            } else {
                spans.push(Span::styled(display_text, theme.base()));
            }
        }

        let p = Paragraph::new(Line::from(spans)).style(theme.base());
        f.render_widget(p, r);
    };

    for (i, &field) in FormField::ALL.iter().enumerate() {
        let (val, is_secret) = match field {
            FormField::Name => (form.name.as_str(), false),
            FormField::Driver => ("", false),
            FormField::Host => (form.host.as_str(), false),
            FormField::Port => (form.port.as_str(), false),
            FormField::User => (form.user.as_str(), false),
            FormField::Password => (form.password.as_str(), true),
            FormField::Database => (form.database.as_str(), false),
        };
        render_input(f, chunks[i], field, val, is_secret);
    }

    // In-flight test status (spinner + label) only while a ping is running.
    // After it completes, show the result banner in the same row so the user
    // sees the outcome right where they're editing.
    if is_testing {
        spinner.render(f, chunks[7], "Testing connection...", theme);
    } else if let Some(result) = &form.last_test_result {
        let (icon, color) = if result.success {
            ("✓ ", theme.success())
        } else {
            ("✗ ", theme.error())
        };
        let line = Line::from(vec![
            Span::styled(icon, color.add_modifier(Modifier::BOLD)),
            Span::styled(&result.message, color),
        ]);
        f.render_widget(
            Paragraph::new(line).alignment(Alignment::Center),
            chunks[7],
        );
    }

    let footer = Line::from(vec![
        Span::styled("[Tab/↓] Next  ", theme.dim()),
        Span::styled("[Shift+Tab/↑] Prev  ", theme.dim()),
        Span::styled("[Ctrl+T] Test  ", theme.accent()),
        Span::styled("[Enter] Save  ", theme.accent()),
        Span::styled("[Esc] Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(footer).alignment(Alignment::Center), chunks[8]);
}

/// Renders a centered confirmation dialog asking the user to confirm
/// deletion of a saved connection. Uses the error/danger palette so the
/// destructive intent is unmistakable. Drawing order: `Clear` first to
/// erase the underlying picker, then the scrim Block (panel style) so the
/// dialog pops over a dimmed background.
pub fn render_confirm_delete_modal(
    f: &mut Frame,
    area: Rect,
    modal: &ConfirmDeleteModal,
    theme: &Theme,
) {
    let width = 56.min(area.width.saturating_sub(4));
    let height = 9.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.error())
        .style(theme.panel())
        .title(" Delete Connection ");

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2), // Warning + connection name
            Constraint::Length(2), // Spacer + destructive hint
            Constraint::Length(1), // Spacer
            Constraint::Length(1), // Action hint
        ])
        .split(inner);

    // 1. Warning line + quoted connection name. Name is bold so it stands
    //    out and the user can verify they're deleting the right one.
    let warn_line = Line::from(vec![
        Span::styled(
            "⚠  Delete saved connection ",
            theme.warning().add_modifier(Modifier::BOLD),
        ),
        Span::styled("'", theme.dim()),
        Span::styled(
            &modal.connection_name,
            theme.error().add_modifier(Modifier::BOLD),
        ),
        Span::styled("'?", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(warn_line), chunks[0]);

    // 2. Explain what gets lost — credentials + saved URL. Not recoverable
    //    unless the user has a config backup, so the wording is direct.
    let explain_line = Line::from(vec![
        Span::styled("This will remove it from ", theme.dim()),
        Span::styled("config.toml", theme.base()),
        Span::styled(" (host, port, user, password, database).", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(explain_line), chunks[1]);

    // 3. Blank spacer.

    // 4. Action hints. Enter (destructive) in error red, Esc in dim.
    let hints = Line::from(vec![
        Span::styled(
            "[Enter] ",
            theme.error().add_modifier(Modifier::BOLD),
        ),
        Span::styled("Delete permanently  ", theme.error()),
        Span::styled("  |  ", theme.dim()),
        Span::styled("[Esc] ", theme.accent()),
        Span::styled("Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(hints).alignment(Alignment::Center), chunks[3]);
}
