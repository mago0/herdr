//! Sidebar sections drawn by a plugin pane that this client runs in a local terminal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::terminal::TerminalRuntime;

/// Rows of the spaces section that stay native: the title above and the footer below.
const SPACES_HEADER_ROWS: u16 = super::WORKSPACE_HEADER_ROWS;
const SPACES_FOOTER_ROWS: u16 = 1;
/// Rows of the agents section that stay native: the rule and the title.
pub(super) const AGENTS_HEADER_ROWS: u16 = 2;
/// A plugin that exits sooner than this did not start correctly.
const QUICK_EXIT: Duration = Duration::from_secs(5);
const MAX_QUICK_EXITS: u8 = 3;
/// How long the client waits before it looks again for a plugin that it could not start.
const START_RETRY: Duration = Duration::from_secs(5);
const SCROLLBACK_LIMIT_BYTES: usize = 64 * 1024;
/// How long the client polls a plugin pane at a short interval after it sent input to it.
const INPUT_ECHO_WINDOW: Duration = Duration::from_millis(400);
const INPUT_ECHO_POLL: Duration = Duration::from_millis(16);
/// How often the program file of a plugin pane is checked for a new build.
const PROGRAM_CHECK: Duration = Duration::from_millis(500);
/// Start of the terminal title a plugin pane sets to name the row under a right-click.
const MENU_TITLE_PREFIX: &str = "herdr-menu;";
/// How long a right-click waits for the plugin pane to name its row.
const MENU_REPLY_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarSection {
    Spaces,
    Agents,
}

impl SidebarSection {
    pub(super) const ALL: [Self; 2] = [Self::Spaces, Self::Agents];

    pub(super) fn index(self) -> usize {
        match self {
            Self::Spaces => 0,
            Self::Agents => 1,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Spaces => "spaces",
            Self::Agents => "agents",
        }
    }
}

/// The process a sidebar plugin runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SidebarPluginCommand {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
}

/// Maps a configured `<plugin id>:<pane id>` to its command. `None` keeps the native section.
pub(crate) type SidebarPluginResolver =
    Arc<dyn Fn(&str, SidebarSection) -> Option<SidebarPluginCommand> + Send + Sync>;

struct Pane {
    spec: String,
    runtime: TerminalRuntime,
    events: mpsc::Receiver<AppEvent>,
    drawn_seq: u64,
    started: Instant,
    /// The last terminal title read, so a menu request is taken once.
    title: Option<String>,
    program: Option<Program>,
}

/// The program file of a plugin pane. A new build of it starts the pane again.
struct Program {
    path: PathBuf,
    started_with: Option<std::time::SystemTime>,
    /// A changed time seen once. The pane starts again when the next check sees the same time,
    /// so a file that is still being written is left alone.
    changed_to: Option<std::time::SystemTime>,
    checked: Instant,
}

impl Program {
    /// `None` for a command found through `PATH`, which is not watched.
    fn of(command: &SidebarPluginCommand, now: Instant) -> Option<Self> {
        let first = command.argv.first().filter(|first| first.contains('/'))?;
        let path = command.cwd.join(first);
        Some(Self {
            started_with: modified(&path),
            path,
            changed_to: None,
            checked: now,
        })
    }

    /// True when the file has a new build that is complete.
    fn rebuilt(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.checked) < PROGRAM_CHECK {
            return false;
        }
        self.checked = now;
        let Some(current) = modified(&self.path).filter(|time| Some(*time) != self.started_with)
        else {
            self.changed_to = None;
            return false;
        };
        if self.changed_to == Some(current) {
            return true;
        }
        self.changed_to = Some(current);
        false
    }
}

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// What a plugin row stands for, as named by its pane after a right-click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarMenuTarget {
    Workspace(String),
    Tab(String),
    Pane(String),
}

/// A right-click on a plugin row, with the screen cell the menu opens at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SidebarMenuRequest {
    pub(super) target: SidebarMenuTarget,
    pub(super) x: u16,
    pub(super) y: u16,
}

fn parse_menu_title(title: &str) -> Option<SidebarMenuTarget> {
    let mut parts = title.strip_prefix(MENU_TITLE_PREFIX)?.split(';');
    let (kind, id) = (parts.next()?, parts.next()?.to_owned());
    match kind {
        "workspace" => Some(SidebarMenuTarget::Workspace(id)),
        "tab" => Some(SidebarMenuTarget::Tab(id)),
        "pane" => Some(SidebarMenuTarget::Pane(id)),
        _ => None,
    }
}

#[derive(Default)]
struct Slot {
    pane: Option<Pane>,
    quick_exits: u8,
    /// The spec that failed too often. A different spec gets a new start.
    gave_up_on: Option<String>,
    /// The spec that did not start, and when to try it again.
    retry: Option<(String, Instant)>,
}

#[derive(Default)]
pub(crate) struct SidebarPlugins {
    resolver: Option<SidebarPluginResolver>,
    slots: [Slot; 2],
    focus: Option<SidebarSection>,
    /// The server-focused pane when the sidebar took the keys. A change gives the keys back.
    focus_anchor: Option<String>,
    input_echo_until: std::cell::Cell<Option<Instant>>,
    /// A right-click sent to a plugin pane that has not named its row yet.
    menu_wait: std::cell::Cell<Option<(SidebarSection, u16, u16, Instant)>>,
    menu_request: Option<SidebarMenuRequest>,
}

/// The part of each expanded sidebar section that a plugin pane draws.
pub(super) fn section_bodies(area: Rect, split_ratio: f32, agents_visible: bool) -> [Rect; 2] {
    let (spaces, agents) = crate::ui::expanded_sidebar_sections(area, split_ratio, agents_visible);
    let spaces_body = Rect::new(
        spaces.x,
        spaces.y.saturating_add(SPACES_HEADER_ROWS),
        spaces.width,
        spaces
            .height
            .saturating_sub(SPACES_HEADER_ROWS + SPACES_FOOTER_ROWS),
    );
    // The last sidebar row holds the collapse control.
    let agents_body = Rect::new(
        agents.x,
        agents.y.saturating_add(AGENTS_HEADER_ROWS),
        agents.width,
        agents.height.saturating_sub(AGENTS_HEADER_ROWS + 1),
    );
    [spaces_body, agents_body]
}

impl SidebarPlugins {
    pub(crate) fn set_resolver(&mut self, resolver: SidebarPluginResolver) {
        self.resolver = Some(resolver);
    }

    /// Start, stop and resize the plugin panes to match the configured specs and the layout.
    pub(super) fn sync(
        &mut self,
        wanted: [Option<(&str, Rect)>; 2],
        cell_size: crate::kitty_graphics::HostCellSize,
        focused_pane_id: Option<&str>,
        now: Instant,
    ) {
        let resolver = self.resolver.clone();
        for (section, wanted) in SidebarSection::ALL.into_iter().zip(wanted) {
            let slot = &mut self.slots[section.index()];
            let Some((spec, rect)) = wanted.filter(|(spec, rect)| {
                !spec.is_empty() && !rect.is_empty() && slot.gave_up_on.as_deref() != Some(*spec)
            }) else {
                slot.pane = None;
                continue;
            };
            if slot.pane.as_ref().is_some_and(|pane| pane.spec != spec) {
                slot.pane = None;
                slot.quick_exits = 0;
            }
            let waiting = slot
                .retry
                .as_ref()
                .is_some_and(|(failed, at)| failed == spec && now < *at);
            if slot.pane.is_none() && !waiting {
                slot.pane = resolver
                    .as_ref()
                    .and_then(|resolve| resolve(spec, section))
                    .and_then(|command| spawn(spec, &command, rect));
                slot.retry = slot
                    .pane
                    .is_none()
                    .then(|| (spec.to_owned(), now + START_RETRY));
            }
            if let Some(pane) = slot.pane.as_mut() {
                if pane.runtime.current_size() != (rect.height, rect.width) {
                    pane.runtime.resize(
                        rect.height,
                        rect.width,
                        cell_size.width_px,
                        cell_size.height_px,
                    );
                }
                pane.drawn_seq = pane.runtime.content_seq();
            }
        }
        if self
            .focus
            .is_some_and(|section| self.slots[section.index()].pane.is_none())
            || (self.focus.is_some() && self.focus_anchor.as_deref() != focused_pane_id)
        {
            self.focus = None;
            self.focus_anchor = None;
        }
    }

    /// Drop panes whose process exited and report whether a pane has output not yet drawn.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let mut repaint = false;
        let menu_wait = self
            .menu_wait
            .get()
            .filter(|(_, _, _, at)| now.duration_since(*at) < MENU_REPLY_WINDOW);
        self.menu_wait.set(menu_wait);
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(pane) = slot.pane.as_mut() else {
                continue;
            };
            let title = pane.runtime.terminal_title();
            if title != pane.title {
                pane.title = title;
                let waiting = menu_wait.filter(|(section, ..)| section.index() == index);
                let target = pane.title.as_deref().and_then(parse_menu_title);
                if let (Some((_, x, y, _)), Some(target)) = (waiting, target) {
                    self.menu_wait.set(None);
                    self.menu_request = Some(SidebarMenuRequest { target, x, y });
                }
            }
            let mut died = false;
            while let Ok(event) = pane.events.try_recv() {
                died |= matches!(event, AppEvent::PaneDied { .. });
            }
            if died {
                if now.duration_since(pane.started) < QUICK_EXIT {
                    slot.quick_exits += 1;
                    if slot.quick_exits >= MAX_QUICK_EXITS {
                        slot.gave_up_on = Some(pane.spec.clone());
                    }
                } else {
                    slot.quick_exits = 0;
                }
                slot.pane = None;
                repaint = true;
            } else if pane
                .program
                .as_mut()
                .is_some_and(|program| program.rebuilt(now))
            {
                slot.pane = None;
                slot.quick_exits = 0;
                repaint = true;
            } else if pane.runtime.content_seq() != pane.drawn_seq {
                repaint = true;
            }
        }
        if self
            .input_echo_until
            .get()
            .is_some_and(|until| now >= until)
        {
            self.input_echo_until.set(None);
        }
        repaint
    }

    pub(super) fn poll_delay(&self, now: Instant) -> Option<Duration> {
        self.input_echo_until
            .get()
            .filter(|until| *until > now)
            .map(|_| INPUT_ECHO_POLL)
    }

    pub(super) fn active(&self, section: SidebarSection) -> bool {
        self.slots[section.index()].pane.is_some()
    }

    #[cfg(test)]
    pub(super) fn gave_up(&self, section: SidebarSection) -> bool {
        self.slots[section.index()].gave_up_on.is_some()
    }

    pub(super) fn focus(&self) -> Option<SidebarSection> {
        self.focus
    }

    pub(super) fn set_focus(
        &mut self,
        section: Option<SidebarSection>,
        focused_pane_id: Option<String>,
    ) -> bool {
        let section = section.filter(|section| self.active(*section));
        let changed = self.focus != section;
        self.focus = section;
        self.focus_anchor = section.and(focused_pane_id);
        changed
    }

    /// Move the keys to the next plugin section, then back to the panes.
    pub(super) fn cycle_focus(&mut self, focused_pane_id: Option<String>) -> bool {
        let order = [
            None,
            Some(SidebarSection::Spaces),
            Some(SidebarSection::Agents),
        ];
        let current = order
            .iter()
            .position(|section| *section == self.focus)
            .unwrap_or(0);
        let next = (1..=order.len())
            .map(|step| order[(current + step) % order.len()])
            .find(|section| section.is_none_or(|section| self.active(section)))
            .flatten();
        self.set_focus(next, focused_pane_id)
    }

    /// Draw the plugin pane of `section` into `rect`. Cells without a background keep the
    /// sidebar background.
    pub(super) fn draw(&self, section: SidebarSection, buffer: &mut Buffer, rect: Rect) {
        let Some(pane) = self.slots[section.index()].pane.as_ref() else {
            return;
        };
        let rect = rect.intersection(buffer.area);
        let (rows, cols) = pane.runtime.current_size();
        let view = Rect::new(0, 0, rect.width.min(cols), rect.height.min(rows));
        if view.is_empty() {
            return;
        }
        let (screen, _) = crate::server::render_stream::render_terminal_virtual(
            &pane.runtime,
            Rect::new(0, 0, cols, rows),
        );
        for y in 0..view.height {
            for x in 0..view.width {
                let (Some(source), Some(target)) = (
                    screen.cell((x, y)),
                    buffer.cell_mut((rect.x + x, rect.y + y)),
                ) else {
                    continue;
                };
                let background = target.bg;
                *target = source.clone();
                if target.bg == Color::Reset {
                    target.bg = background;
                }
            }
        }
    }

    /// Send a mouse event at a position inside `rect` to the plugin pane of `section`.
    pub(super) fn send_mouse(
        &self,
        section: SidebarSection,
        rect: Rect,
        mouse: MouseEvent,
        now: Instant,
    ) {
        let Some(pane) = self.slots[section.index()].pane.as_ref() else {
            return;
        };
        let position = crate::input::mouse::Position::Cell {
            column: mouse.column.saturating_sub(rect.x),
            row: mouse.row.saturating_sub(rect.y),
        };
        let bytes =
            match mouse.kind {
                MouseEventKind::Down(_) | MouseEventKind::Up(_) => pane
                    .runtime
                    .encode_mouse_button(mouse.kind, position, mouse.modifiers),
                MouseEventKind::Drag(_) | MouseEventKind::Moved => pane
                    .runtime
                    .encode_mouse_motion(mouse.kind, position, mouse.modifiers),
                MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight => pane
                    .runtime
                    .encode_mouse_wheel(mouse.kind, position, mouse.modifiers)
                    .or_else(|| pane.runtime.encode_alternate_scroll(mouse.kind)),
            };
        if let Some(bytes) = bytes {
            let _ = pane.runtime.try_send_bytes(bytes.into());
            self.input_echo_until.set(Some(now + INPUT_ECHO_WINDOW));
            if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Right) {
                self.menu_wait
                    .set(Some((section, mouse.column, mouse.row, now)));
            }
        }
    }

    /// The menu a plugin pane asked for since the last call.
    pub(super) fn take_menu_request(&mut self) -> Option<SidebarMenuRequest> {
        self.menu_request.take()
    }

    /// Send a key to the focused plugin pane. Returns false when no plugin pane has the keys.
    pub(super) fn send_key(&self, key: crate::input::TerminalKey, now: Instant) -> bool {
        let Some(pane) = self
            .focus
            .and_then(|section| self.slots[section.index()].pane.as_ref())
        else {
            return false;
        };
        let bytes = pane.runtime.encode_terminal_key(key);
        if !bytes.is_empty() {
            let _ = pane.runtime.try_send_bytes(bytes.into());
            self.input_echo_until.set(Some(now + INPUT_ECHO_WINDOW));
        }
        true
    }
}

impl SidebarPlugins {
    /// Send typed or pasted text to the focused plugin pane. Returns false when no plugin
    /// pane has the keys.
    pub(super) fn send_text(&self, text: &str, now: Instant) -> bool {
        let Some(pane) = self
            .focus
            .and_then(|section| self.slots[section.index()].pane.as_ref())
        else {
            return false;
        };
        let _ = pane
            .runtime
            .try_send_bytes(bytes::Bytes::copy_from_slice(text.as_bytes()));
        self.input_echo_until.set(Some(now + INPUT_ECHO_WINDOW));
        true
    }
}

fn spawn(spec: &str, command: &SidebarPluginCommand, rect: Rect) -> Option<Pane> {
    // The runtime reads its PTY on tasks of the client's async runtime.
    tokio::runtime::Handle::try_current().ok()?;
    let (events_tx, events) = mpsc::channel(64);
    let runtime = TerminalRuntime::spawn_argv_command(
        crate::layout::PaneId::alloc(),
        rect.height,
        rect.width,
        command.cwd.clone(),
        &command.argv,
        &crate::pane::PaneLaunchEnv::from_extra(command.env.clone()),
        crate::pane::AgentDetection::Disabled,
        SCROLLBACK_LIMIT_BYTES,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events_tx,
        Arc::new(Notify::new()),
        Arc::new(crate::render_signal::RenderSignal::new()),
    );
    match runtime {
        Ok(runtime) => Some(Pane {
            spec: spec.to_owned(),
            runtime,
            events,
            drawn_seq: 0,
            started: Instant::now(),
            title: None,
            program: Program::of(command, Instant::now()),
        }),
        Err(error) => {
            tracing::warn!(spec, %error, "sidebar plugin pane did not start");
            None
        }
    }
}

/// Resolve sidebar plugin specs against the plugins installed on this machine.
pub(crate) fn installed_plugin_resolver() -> SidebarPluginResolver {
    Arc::new(|spec, section| {
        let (plugin_id, pane_id) = spec.split_once(':').unwrap_or((spec, ""));
        let plugins = crate::persist::plugin_registry::load();
        let plugin = plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.plugin_id == plugin_id)?;
        let pane = plugin
            .panes
            .iter()
            .find(|pane| pane_id.is_empty() || pane.id == pane_id)?;
        if let Err(error) = crate::plugin_paths::ensure_plugin_user_dirs(&plugin.plugin_id) {
            tracing::warn!(spec, %error, "sidebar plugin directories are not available");
            return None;
        }
        let mut env = vec![
            ("HERDR_PLUGIN_ROOT".to_owned(), plugin.plugin_root.clone()),
            (
                "HERDR_PLUGIN_CONFIG_DIR".to_owned(),
                crate::plugin_paths::plugin_config_dir(&plugin.plugin_id)
                    .display()
                    .to_string(),
            ),
            (
                "HERDR_PLUGIN_STATE_DIR".to_owned(),
                crate::plugin_paths::plugin_state_dir(&plugin.plugin_id)
                    .display()
                    .to_string(),
            ),
            (
                crate::api::SOCKET_PATH_ENV_VAR.to_owned(),
                crate::api::socket_path().display().to_string(),
            ),
            ("HERDR_ENV".to_owned(), "1".to_owned()),
            ("HERDR_PLUGIN_ID".to_owned(), plugin.plugin_id.clone()),
            ("HERDR_PLUGIN_ENTRYPOINT_ID".to_owned(), pane.id.clone()),
            ("HERDR_PLUGIN_CONTEXT_JSON".to_owned(), "{}".to_owned()),
            ("HERDR_PLUGIN_PLACEMENT".to_owned(), "sidebar".to_owned()),
            (
                "HERDR_SIDEBAR_SECTION".to_owned(),
                section.name().to_owned(),
            ),
        ];
        if let Ok(current_exe) = crate::platform::launch_executable() {
            env.push((
                "HERDR_BIN_PATH".to_owned(),
                current_exe.display().to_string(),
            ));
        }
        Some(SidebarPluginCommand {
            argv: pane.command.clone(),
            cwd: PathBuf::from(&plugin.plugin_root),
            env,
        })
    })
}

/// Draw the agents section with its native title rows and the plugin pane below them.
pub(super) fn render_agents_section(
    buffer: &mut Buffer,
    area: Rect,
    config: &super::ClientShellConfig,
    plugins: &SidebarPlugins,
    hits: &mut super::ShellHitMap,
) {
    if area.height <= AGENTS_HEADER_ROWS {
        return;
    }
    let palette = &config.palette;
    super::render::put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        ratatui::style::Style::default().fg(palette.surface_dim),
    );
    let focused = plugins.focus() == Some(SidebarSection::Agents);
    super::render::put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " agents",
        ratatui::style::Style::default()
            .fg(if focused {
                palette.accent
            } else {
                palette.overlay0
            })
            .add_modifier(ratatui::style::Modifier::BOLD),
    );
    let body = Rect::new(
        area.x,
        area.y + AGENTS_HEADER_ROWS,
        area.width,
        area.height.saturating_sub(AGENTS_HEADER_ROWS + 1),
    );
    hits.sidebar_plugins[SidebarSection::Agents.index()] = body;
    plugins.draw(SidebarSection::Agents, buffer, body);
}

impl super::ClientShellState {
    pub(crate) fn set_sidebar_plugin_resolver(&mut self, resolver: SidebarPluginResolver) {
        self.sidebar_plugins.set_resolver(resolver);
    }

    /// Match the plugin panes to the sidebar that the next frame draws.
    pub(super) fn sync_sidebar_plugins(&mut self, sidebar: Rect) {
        let expanded = sidebar.width > 1 && !self.sidebar_collapsed && self.endpoints.len() == 1;
        let bodies = section_bodies(
            sidebar,
            self.sidebar_section_split,
            self.agent_panel_visible,
        );
        let focused_pane_id = self.focused_pane_id();
        let wanted = [
            expanded.then(|| (self.config.spaces.plugin.as_str(), bodies[0])),
            (expanded && self.agent_panel_visible)
                .then(|| (self.config.agents.plugin.as_str(), bodies[1])),
        ];
        self.sidebar_plugins.sync(
            wanted,
            self.graphics_cell_size,
            focused_pane_id.as_deref(),
            Instant::now(),
        );
    }

    pub(crate) fn tick_sidebar_plugins(&mut self, now: Instant) -> bool {
        let mut repaint = self.sidebar_plugins.tick(now);
        if let Some(request) = self.sidebar_plugins.take_menu_request() {
            if self.overlay.is_none() && self.config.mouse_capture {
                let (x, y) = (request.x, request.y);
                match request.target {
                    SidebarMenuTarget::Workspace(id) => self.open_workspace_context_menu(id, x, y),
                    SidebarMenuTarget::Tab(id) => self.open_tab_context_menu(id, x, y),
                    SidebarMenuTarget::Pane(id) => self.open_pane_context_menu(id, x, y),
                }
                repaint |= self.overlay.is_some();
            }
        }
        repaint
    }
}
