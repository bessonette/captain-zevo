//! The main window: sidebar, project list, project detail and system pages.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, IconName, Sizable, StyledExt, TitleBar, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zevo_core::health::Listening;
use zevo_core::model::{LOCAL_HOST_ID, Target};
use zevo_core::reconcile::ContainerState;

use crate::Refresh;
use crate::backend::{Backend, ProjectView, Snapshot};

mod add;
mod hosts;
mod setup;

const LOGO: &[u8] = include_bytes!("../../../assets/logo.svg");
/// One-colour logo, tinted from the theme where it is used.
const LOGO_MONO: &[u8] = include_bytes!("../../../assets/logo-mono.svg");

/// What a menu bar item or shortcut asks the main window to do.
#[derive(Clone, Copy)]
pub enum MenuCommand {
    About,
    Settings,
    AddProject,
    Refresh,
    Projects,
    Traefik,
    Hosts,
}

/// Index of the Logs tab on a project page.
const LOGS_TAB: usize = 2;
/// Lines of container output fetched for the Logs tab.
const LOG_LINES: usize = 300;

#[derive(Clone, PartialEq)]
enum Page {
    /// Project list, optionally limited to one host.
    Projects(Option<String>),
    Project(String),
    Traefik,
    Settings,
    Add,
    Hosts,
}

pub struct Shell {
    backend: Arc<dyn Backend>,
    demo: bool,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    loading: bool,
    /// A change arrived while a load was running.
    reload_again: bool,
    page: Page,
    tab: usize,
    search: Entity<InputState>,
    logo: Arc<Image>,
    focus: FocusHandle,
    add: Option<add::AddForm>,
    setup: setup::SetupState,
    host_form: Option<hosts::HostForm>,
    /// Projects whose full address list is shown in the project list.
    expanded: std::collections::BTreeSet<String>,
    /// Project slug to the action running on it ("Starting", ...).
    busy: std::collections::BTreeMap<String, &'static str>,
    /// Why the last action on a project failed, shown on its page.
    action_error: Option<(String, String)>,
    /// Output shown on the Logs tab, for the named project.
    logs: Option<(String, Result<String, String>)>,
    logs_loading: bool,
    _search_changed: Subscription,
}

impl Shell {
    pub fn new(
        backend: Arc<dyn Backend>,
        demo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search projects"));
        let _search_changed = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut shell = Self {
            backend,
            demo,
            snapshot: None,
            error: None,
            loading: false,
            reload_again: false,
            page: Page::Projects(None),
            tab: 0,
            search,
            logo: Arc::new(Image::from_bytes(ImageFormat::Svg, LOGO.to_vec())),
            focus: cx.focus_handle(),
            add: None,
            setup: setup::SetupState::default(),
            host_form: None,
            expanded: Default::default(),
            busy: Default::default(),
            action_error: None,
            logs: None,
            logs_loading: false,
            _search_changed,
        };
        shell.reload(cx);
        // Containers come and go outside the app, so keep the picture fresh.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        shell
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.loading {
            self.reload_now(cx);
        }
    }

    /// Reloads right after a change. Loads never overlap: two at once would
    /// reconcile concurrently, and the older one could finish last and show
    /// (and write) stale state. If one is running, another follows it.
    fn reload_now(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.reload_again = true;
            return;
        }
        self.loading = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { backend.load() }).await;
            _ = this.update(cx, |this, cx| {
                this.loading = false;
                if std::mem::take(&mut this.reload_again) {
                    // Something changed while this load ran; its result is
                    // already out of date.
                    this.reload_now(cx);
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        if snapshot.needs_setup() && !this.setup_searched() {
                            this.find_traefik(cx);
                        }
                        this.snapshot = Some(snapshot);
                        this.error = None;
                        if let (Page::Project(slug), LOGS_TAB) = (&this.page, this.tab) {
                            this.load_logs(slug.clone(), cx);
                        }
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_editor(
        &mut self,
        slug: String,
        editor: zevo_core::ide::Editor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.open_in_editor(&slug, editor) })
                .await;
            _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => window.push_notification(format!("Opening in {}…", editor.name()), cx),
                Err(e) => {
                    window.push_notification(format!("Could not open {}: {e}", editor.name()), cx)
                }
            });
        })
        .detach();
    }

    /// Runs a container action in the background, marking the project busy.
    fn run_action(
        &mut self,
        slug: String,
        label: &'static str,
        done: &'static str,
        work: impl FnOnce(&dyn Backend, &str) -> Result<(), String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy.insert(slug.clone(), label);
        self.action_error = None;
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let target = slug.clone();
            let result = cx
                .background_spawn(async move { work(backend.as_ref(), &target) })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                this.busy.remove(&slug);
                match result {
                    Ok(()) => window.push_notification(format!("{done} {slug}"), cx),
                    // Build output can be long; show it on the page, not in a toast.
                    Err(e) => this.action_error = Some((slug.clone(), e)),
                }
                this.reload_now(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_logs(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.logs_loading {
            return;
        }
        self.logs_loading = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let target = slug.clone();
            let result = cx
                .background_spawn(async move { backend.logs(&target, LOG_LINES) })
                .await;
            _ = this.update(cx, |this, cx| {
                this.logs_loading = false;
                this.logs = Some((slug, result));
                cx.notify();
            });
        })
        .detach();
    }

    pub fn menu_command(
        &mut self,
        command: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            MenuCommand::Refresh => self.reload(cx),
            MenuCommand::Projects => self.go(Page::Projects(None), cx),
            MenuCommand::Traefik => self.go(Page::Traefik, cx),
            MenuCommand::Settings => self.go(Page::Settings, cx),
            MenuCommand::Hosts => self.open_hosts(window, cx),
            MenuCommand::AddProject => self.open_add(window, cx),
            MenuCommand::About => {
                let logo = self.logo.clone();
                window.open_dialog(cx, move |dialog, _, cx| {
                    dialog.child(
                        v_flex()
                            .gap_3()
                            .items_center()
                            .pb_2()
                            .child(img(logo.clone()).size(px(96.)))
                            .child(
                                v_flex()
                                    .items_center()
                                    .child(div().text_lg().font_semibold().child("Captain Zevo"))
                                    .child(
                                        div()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                                    ),
                            )
                            .child(div().text_center().child(
                                "Gives every devcontainer project a stable local address by managing Traefik routes.",
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Open source under the MIT and Apache-2.0 licenses."),
                            ),
                    )
                });
            }
        }
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.tab = 0;
        cx.notify();
    }

    fn remove(&mut self, slug: String, window: &mut Window, cx: &mut Context<Self>) {
        match self.backend.remove_project(&slug) {
            Ok(()) => {
                window.push_notification(format!("Removed {slug}"), cx);
                self.page = Page::Projects(None);
                self.reload(cx);
            }
            Err(e) => window.push_notification(format!("Could not remove {slug}: {e}"), cx),
        }
        cx.notify();
    }
}

/// How many of a project's addresses the project list shows before the
/// rest are folded away.
const ADDRESSES_SHOWN: usize = 2;

/// The name a host is shown by: the one it was given, not its id.
fn host_label(snapshot: &Snapshot, host: &str) -> String {
    if host == LOCAL_HOST_ID {
        return "This machine".to_string();
    }
    snapshot
        .hosts
        .iter()
        .find(|h| h.host.id == host)
        .map_or_else(|| host.to_string(), |h| h.host.name.clone())
}

fn needs_network(view: &ProjectView) -> bool {
    view.project
        .routes
        .iter()
        .any(|r| r.target == Target::Container)
}

/// Status pill: a tinted background and a coloured dot, with normal text so
/// it stays readable in light and dark themes.
fn status_pill(view: &ProjectView, cx: &App) -> Div {
    let theme = cx.theme();
    let (color, label) = match view.status.state {
        ContainerState::Running if view.status.attached || !needs_network(view) => {
            (theme.success, "Running")
        }
        ContainerState::Running => (theme.warning, "Not attached"),
        ContainerState::Stopped => (theme.muted_foreground, "Stopped"),
        ContainerState::Missing => (theme.danger, "No container"),
        ContainerState::Unknown => (theme.muted_foreground, "Unknown"),
    };
    h_flex()
        .flex_shrink_0()
        .gap_1p5()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.14))
        .text_xs()
        .font_medium()
        .text_color(theme.foreground)
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(label)
}

/// Uppercase column or section caption.
fn caption(text: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
}

fn mono(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(cx.theme().mono_font_size)
        .child(text.into())
}

/// Monospace text that opens `url` in the browser when clicked. The click
/// does not also trigger whatever the text sits in (such as a table row).
fn link(id: impl Into<SharedString>, label: impl Into<SharedString>, url: String, cx: &App) -> Div {
    h_flex().child(
        mono(label, cx)
            .id(ElementId::Name(id.into()))
            .cursor_pointer()
            .text_color(cx.theme().link)
            .hover(|style| style.underline())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                cx.open_url(&url);
            }),
    )
}

/// The address of a hostname served by the Traefik that Zevo uses.
fn address_url(snapshot: &Snapshot, host: &str) -> String {
    let (tls, port) = match &snapshot.settings.traefik {
        Some(t) => (t.tls, t.http_port),
        None => (false, 80),
    };
    let scheme = if tls { "https" } else { "http" };
    match (tls, port) {
        (true, 443) | (false, 80) => format!("{scheme}://{host}"),
        _ => format!("{scheme}://{host}:{port}"),
    }
}

/// A label and its value on one line.
fn field(label: &'static str, value: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .items_start()
        .gap_4()
        .child(
            div()
                .w(px(130.))
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value))
}

impl Shell {
    fn render_sidebar(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let mut hosts: Vec<String> = snapshot
            .projects
            .iter()
            .map(|p| p.project.host.clone())
            .collect();
        hosts.sort_by_key(|h| (h != LOCAL_HOST_ID, h.clone()));
        hosts.dedup();

        let count = |host: Option<&str>| {
            snapshot
                .projects
                .iter()
                .filter(|p| host.is_none_or(|h| p.project.host == h))
                .count()
        };
        let in_projects = matches!(self.page, Page::Projects(_) | Page::Project(_));
        let current_filter = match &self.page {
            Page::Projects(filter) => filter.clone(),
            _ => None,
        };

        let mut project_items = vec![
            SidebarMenuItem::new("All projects")
                .icon(IconName::LayoutDashboard)
                .active(in_projects && current_filter.is_none())
                .suffix({
                    let n = count(None);
                    move |_, cx| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(n.to_string())
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Projects(None), cx))),
        ];
        for host in hosts {
            let n = count(Some(&host));
            let target = host.clone();
            project_items.push(
                SidebarMenuItem::new(host_label(snapshot, &host))
                    .icon(if host == LOCAL_HOST_ID {
                        IconName::HardDrive
                    } else {
                        IconName::Network
                    })
                    .active(current_filter.as_deref() == Some(host.as_str()))
                    .suffix(move |_, cx| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(n.to_string())
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go(Page::Projects(Some(target.clone())), cx)
                    })),
            );
        }

        let traefik_line = match &snapshot.settings.traefik {
            Some(t) => format!(
                "{} · port {}{}",
                t.entrypoint,
                t.http_port,
                if t.tls { " · HTTPS" } else { "" }
            ),
            None => "Traefik not set up".to_string(),
        };
        let runtime_ok = snapshot.runtime.is_ok();

        Sidebar::new("nav")
            .collapsible(false)
            .w(px(216.))
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_2()
                        .child(img(self.logo.clone()).size(px(36.)))
                        .child(div().text_lg().font_semibold().child("Captain Zevo")),
                ),
            )
            .child(SidebarGroup::new("Projects").child(SidebarMenu::new().children(project_items)))
            .child(
                SidebarGroup::new("System").child(
                    SidebarMenu::new()
                        .child(
                            SidebarMenuItem::new("Traefik")
                                .icon(IconName::Globe)
                                .active(self.page == Page::Traefik)
                                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Traefik, cx))),
                        )
                        .child(
                            SidebarMenuItem::new("Hosts")
                                .icon(IconName::Network)
                                .active(self.page == Page::Hosts)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_hosts(window, cx)),
                                ),
                        )
                        .child(
                            SidebarMenuItem::new("Settings")
                                .icon(IconName::Settings)
                                .active(self.page == Page::Settings)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.go(Page::Settings, cx)),
                                ),
                        ),
                ),
            )
            .footer(
                SidebarFooter::new().child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().size(px(7.)).rounded_full().bg(if runtime_ok {
                                    cx.theme().success
                                } else {
                                    cx.theme().danger
                                }))
                                .child(if runtime_ok {
                                    "Runtime connected"
                                } else {
                                    "Runtime unreachable"
                                }),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(traefik_line),
                        ),
                ),
            )
    }

    fn page_header(&self, title: impl IntoElement, actions: impl IntoElement, cx: &App) -> Div {
        h_flex()
            .flex_shrink_0()
            .h(px(52.))
            .px_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .text_lg()
                    .font_semibold()
                    .child(title),
            )
            .child(actions)
    }

    fn render_projects(
        &self,
        snapshot: &Snapshot,
        filter: &Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let query = self.search.read(cx).value().to_lowercase();
        let rows: Vec<&ProjectView> = snapshot
            .projects
            .iter()
            .filter(|p| filter.as_ref().is_none_or(|h| &p.project.host == h))
            .filter(|p| {
                query.is_empty()
                    || p.project.name.to_lowercase().contains(&query)
                    || p.project.slug.contains(&query)
                    || p.project
                        .tags
                        .iter()
                        .any(|t| t.to_lowercase().contains(&query))
            })
            .collect();

        let title = filter
            .as_deref()
            .map(|host| host_label(snapshot, host))
            .unwrap_or_else(|| "All projects".to_string());
        let header = self.page_header(
            title,
            h_flex()
                .gap_2()
                .child(
                    div().w(px(220.)).child(
                        Input::new(&self.search)
                            .prefix(IconName::Search)
                            .cleanable(true)
                            .small(),
                    ),
                )
                .child(
                    Button::new("refresh")
                        .ghost()
                        .small()
                        .icon(IconName::RefreshCw)
                        .tooltip("Refresh")
                        .loading(self.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .child(
                    Button::new("add-project")
                        .primary()
                        .small()
                        .icon(IconName::Plus)
                        .label("Add project")
                        .on_click(cx.listener(|this, _, window, cx| this.open_add(window, cx))),
                ),
            cx,
        );

        let columns = h_flex()
            .flex_shrink_0()
            .px_4()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(caption("Status", cx).w(px(118.)))
            .child(caption("Project", cx).flex_1())
            .child(caption("Address", cx).flex_1())
            .child(caption("Host", cx).w(px(120.)))
            .child(caption("Type", cx).w(px(90.)));

        let body = if rows.is_empty() {
            let message = if snapshot.projects.is_empty() {
                "No projects yet. Choose Add project to register a folder."
            } else {
                "No projects match. Clear the search to see them all."
            };
            v_flex()
                .p_8()
                .pt_16()
                .gap_4()
                .items_center()
                .text_color(cx.theme().muted_foreground)
                // The logo only greets an empty registry, not an empty search.
                .when(snapshot.projects.is_empty(), |col| {
                    col.child(img(self.logo.clone()).size(px(112.)))
                })
                .child(message)
        } else {
            v_flex().children(rows.into_iter().enumerate().map(|(ix, view)| {
                let slug = view.project.slug.clone();
                let addresses = if view.project.routes.is_empty() {
                    v_flex()
                        .text_color(cx.theme().muted_foreground)
                        .child("no routes")
                } else {
                    let total = view.project.routes.len();
                    let expanded = self.expanded.contains(&view.project.slug);
                    let shown = if expanded || total <= ADDRESSES_SHOWN + 1 {
                        total
                    } else {
                        ADDRESSES_SHOWN
                    };
                    let toggle = (total > ADDRESSES_SHOWN + 1).then(|| {
                        let slug = view.project.slug.clone();
                        h_flex().child(
                            div()
                                .id(("addresses-toggle", ix))
                                .text_sm()
                                .cursor_pointer()
                                .text_color(cx.theme().muted_foreground)
                                .hover(|style| style.underline())
                                .child(if expanded {
                                    "Show fewer".to_string()
                                } else {
                                    format!("+ {} more", total - shown)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.expanded.remove(&slug) {
                                        this.expanded.insert(slug.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                    });
                    v_flex()
                        .gap_0p5()
                        .children(view.project.routes.iter().enumerate().take(shown).map(
                            |(route_ix, r)| {
                                let host = r
                                    .hostname(&view.project.slug, &snapshot.settings.domain_suffix);
                                let url = view
                                    .status
                                    .urls
                                    .get(route_ix)
                                    .cloned()
                                    .unwrap_or_else(|| address_url(snapshot, &host));
                                h_flex()
                                    .gap_2()
                                    .child(link(
                                        format!("list-{}-{route_ix}", view.project.slug),
                                        format!("{host}{}", r.path_prefix.as_deref().unwrap_or("")),
                                        url,
                                        cx,
                                    ))
                                    .child(
                                        mono(format!("→ {}", r.port), cx)
                                            .text_color(cx.theme().muted_foreground),
                                    )
                            },
                        ))
                        .children(toggle)
                };
                h_flex()
                    .id(("project-row", ix))
                    .px_4()
                    .py_2()
                    .gap_3()
                    .items_start()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.go(Page::Project(slug.clone()), cx)),
                    )
                    .child(h_flex().w(px(118.)).child(status_pill(view, cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_medium().child(view.project.name.clone()))
                            .child(
                                mono(view.project.slug.clone(), cx)
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .child(div().flex_1().min_w_0().child(addresses))
                    .child(
                        div()
                            .w(px(120.))
                            .child(host_label(snapshot, &view.project.host)),
                    )
                    .child(
                        h_flex().w(px(90.)).child(
                            Tag::secondary()
                                .small()
                                .outline()
                                .child(format!("{:?}", view.project.kind).to_lowercase()),
                        ),
                    )
            }))
        };

        // Devcontainers that are not registered, offered for adding: all of
        // them, or only those on the host being looked at.
        let discovered: Vec<_> = snapshot
            .discovered
            .iter()
            .filter(|d| filter.as_ref().is_none_or(|host| &d.host == host))
            .collect();
        let show_found = query.is_empty() && !discovered.is_empty();
        let found = show_found.then(|| {
            v_flex()
                .px_4()
                .pt_6()
                .pb_4()
                .child(caption("Found, not in Zevo yet", cx))
                .child(
                    div()
                        .pb_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "Devcontainers on this machine and your hosts that are not registered.",
                        ),
                )
                .children(discovered.into_iter().enumerate().map(|(ix, d)| {
                    let host = d.host.clone();
                    let folder = d.folder.to_string_lossy().into_owned();
                    let name = d
                        .folder
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| folder.clone());
                    h_flex()
                        .py_2()
                        .gap_3()
                        .border_t_1()
                        .border_color(cx.theme().border.opacity(0.6))
                        .child(
                            h_flex()
                                .w(px(102.))
                                .gap_1p5()
                                .text_sm()
                                .child(div().size(px(6.)).rounded_full().bg(if d.running {
                                    cx.theme().success
                                } else {
                                    cx.theme().muted_foreground
                                }))
                                .child(if d.running { "Running" } else { "Stopped" }),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().font_medium().child(name))
                                .child(
                                    mono(folder.clone(), cx)
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                        .child(div().w(px(120.)).child(host_label(snapshot, &d.host)))
                        .child(
                            Button::new(("add-found", ix))
                                .small()
                                .icon(IconName::Plus)
                                .label("Add")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_add_for(host.clone(), folder.clone(), window, cx)
                                })),
                        )
                }))
        });

        v_flex().size_full().child(header).child(columns).child(
            div().flex_1().overflow_hidden().child(
                v_flex()
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(body)
                    .children(found),
            ),
        )
    }

    /// Start, Stop and Rebuild for a project, according to its state.
    fn lifecycle_buttons(&self, view: &ProjectView, cx: &mut Context<Self>) -> Vec<Button> {
        let slug = view.project.slug.clone();
        if let Some(label) = self.busy.get(&slug) {
            return vec![
                Button::new("busy")
                    .small()
                    .outline()
                    .loading(true)
                    .label(*label),
            ];
        }
        let can_build = view.build_blocker.is_none();
        let mut buttons = Vec::new();
        match view.status.state {
            ContainerState::Running => buttons.push({
                let slug = slug.clone();
                Button::new("stop")
                    .small()
                    .outline()
                    .icon(IconName::Pause)
                    .label("Stop")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_action(
                            slug.clone(),
                            "Stopping…",
                            "Stopped",
                            |b, s| b.stop(s),
                            window,
                            cx,
                        )
                    }))
            }),
            // A stopped container can always be started; a missing one has to be built.
            ContainerState::Stopped | ContainerState::Missing
                if view.status.state == ContainerState::Stopped || can_build =>
            {
                let building = view.status.state == ContainerState::Missing;
                let slug = slug.clone();
                buttons.push(
                    Button::new("start")
                        .small()
                        .outline()
                        .icon(IconName::Play)
                        .label(if building { "Build and start" } else { "Start" })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_action(
                                slug.clone(),
                                if building {
                                    "Building…"
                                } else {
                                    "Starting…"
                                },
                                "Started",
                                |b, s| b.start(s),
                                window,
                                cx,
                            )
                        })),
                )
            }
            _ => {}
        }
        if can_build && view.status.state != ContainerState::Missing {
            buttons.push(
                Button::new("rebuild")
                    .small()
                    .outline()
                    .icon(IconName::RefreshCw)
                    .label("Rebuild")
                    .tooltip("Recreate the container from its devcontainer.json")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_action(
                            slug.clone(),
                            "Rebuilding…",
                            "Rebuilt",
                            |b, s| b.rebuild(s),
                            window,
                            cx,
                        )
                    })),
            );
        }
        buttons
    }

    fn render_project(
        &self,
        snapshot: &Snapshot,
        view: &ProjectView,
        cx: &mut Context<Self>,
    ) -> Div {
        let p = &view.project;
        let first_url = view.status.urls.first().cloned();
        let slug = p.slug.clone();

        let header = self.page_header(
            h_flex()
                .gap_2()
                .child(
                    Button::new("back")
                        .ghost()
                        .small()
                        .icon(IconName::ArrowLeft)
                        .tooltip("All projects")
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Projects(None), cx))),
                )
                .child(p.name.clone())
                .child(status_pill(view, cx)),
            h_flex()
                .gap_2()
                .children(self.lifecycle_buttons(view, cx))
                .child(
                    Button::new("remove")
                        .small()
                        .outline()
                        .label("Remove")
                        .on_click(cx.listener(move |_, _, window, cx| {
                            let shell = cx.entity().downgrade();
                            let slug = slug.clone();
                            window.open_dialog(cx, move |dialog, _, _| {
                                let shell = shell.clone();
                                let slug = slug.clone();
                                dialog
                                .title(format!("Remove {slug}?"))
                                .child(
                                    "Zevo forgets this project and deletes its Traefik route file. \
                                         The folder and its container are not touched.",
                                )
                                .footer(
                                    DialogFooter::new()
                                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                                        .child(DialogAction::new().child(
                                            Button::new("confirm-remove").danger().label("Remove"),
                                        )),
                                )
                                .on_ok(move |_, window, cx| {
                                    _ = shell.update(cx, |this, cx| {
                                        this.remove(slug.clone(), window, cx)
                                    });
                                    true
                                })
                            });
                        })),
                )
                .when_some(first_url, |row, url| {
                    row.child(
                        Button::new("open")
                            .small()
                            .outline()
                            .icon(IconName::ExternalLink)
                            .label("Open in browser")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                })
                .when_some(snapshot.editor(), |row, editor| {
                    let slug = p.slug.clone();
                    row.child(
                        Button::new("open-editor")
                            .primary()
                            .small()
                            .icon(IconName::SquareTerminal)
                            .label(format!("Open in {}", editor.name()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_editor(slug.clone(), editor, window, cx)
                            })),
                    )
                }),
            cx,
        );

        let tabs = TabBar::new("project-tabs")
            .underline()
            .selected_index(self.tab)
            .children([
                Tab::new().label("Overview"),
                Tab::new().label(format!("Routes ({})", p.routes.len())),
                Tab::new().label("Logs"),
                Tab::new().label("Config"),
            ])
            .on_click(cx.listener({
                let slug = p.slug.clone();
                move |this, ix: &usize, _, cx| {
                    this.tab = *ix;
                    if *ix == LOGS_TAB {
                        this.logs = None;
                        this.load_logs(slug.clone(), cx);
                    }
                    cx.notify();
                }
            }));

        let body = match self.tab {
            0 => {
                let notice = match view.status.state {
                    ContainerState::Running if !view.status.attached && needs_network(view) => {
                        Some(Alert::warning(
                            "state",
                            format!(
                                "The container is running but is not on the {} network, so its address will not respond yet.",
                                snapshot.settings.network
                            ),
                        ))
                    }
                    ContainerState::Missing => Some(Alert::error(
                        "state",
                        "No container exists for this folder. Open the project in your editor to build it.",
                    )),
                    _ => None,
                };
                let loopback: Vec<String> = p
                    .routes
                    .iter()
                    .zip(&view.status.backends)
                    .filter(|(_, b)| **b == Listening::LoopbackOnly)
                    .map(|(r, _)| r.port.to_string())
                    .collect();
                v_flex()
                    .gap_5()
                    .children(notice)
                    .when_some(
                        self.action_error.as_ref().filter(|(slug, _)| slug == &p.slug).map(|(_, e)| e.clone()),
                        |col, e| {
                            col.child(
                                v_flex()
                                    .gap_2()
                                    .p_3()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(cx.theme().danger.opacity(0.5))
                                    .bg(cx.theme().danger.opacity(0.08))
                                    .child(div().font_medium().child("The last action did not finish"))
                                    .children(e.lines().map(|line| mono(line.to_string(), cx))),
                            )
                        },
                    )
                    .when(!loopback.is_empty(), |col| {
                        col.child(Alert::warning(
                            "loopback",
                            format!(
                                "Port {} listens only on 127.0.0.1 inside the container, so Traefik cannot reach it. \
                                 Start that server on 0.0.0.0 (dev servers usually have a --host 0.0.0.0 option).",
                                loopback.join(", ")
                            ),
                        ))
                    })
                    .child(
                        v_flex()
                            .gap_2()
                            .child(caption("Project", cx))
                            .child(field("Folder", mono(p.path.display().to_string(), cx), cx))
                            .child(field("Host", host_label(snapshot, &p.host), cx))
                            .child(field(
                                "Devcontainer",
                                format!("{:?}", p.kind).to_lowercase(),
                                cx,
                            ))
                            .child(field(
                                "Container",
                                mono(
                                    view.status.container.clone().unwrap_or_else(|| "—".into()),
                                    cx,
                                ),
                                cx,
                            ))
                            .child(field(
                                "Tags",
                                if p.tags.is_empty() {
                                    "—".to_string()
                                } else {
                                    p.tags.join(", ")
                                },
                                cx,
                            )),
                    )
                    .child(v_flex().gap_2().child(caption("Addresses", cx)).children(
                        if view.status.urls.is_empty() {
                            vec![div().text_color(cx.theme().muted_foreground).child("No routes yet. Add one on the Routes tab to give this project an address.")]
                        } else {
                            view.status
                                .urls
                                .iter()
                                .enumerate()
                                .map(|(ix, url)| {
                                    let target = url.clone();
                                    div().child(
                                        h_flex()
                                            .id(("url", ix))
                                            .cursor_pointer()
                                            .on_click(move |_, _, cx| cx.open_url(&target))
                                            .child(
                                                mono(url.clone(), cx).text_color(cx.theme().link),
                                            ),
                                    )
                                })
                                .collect()
                        },
                    ))
            }
            1 => v_flex()
                .child(
                    h_flex()
                        .py_2()
                        .gap_3()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(caption("Address", cx).flex_1())
                        .child(caption("Sends to", cx).flex_1())
                        .child(caption("Port", cx).w(px(70.)))
                        .child(caption("Path", cx).w(px(90.)))
                        .child(caption("App", cx).w(px(150.)))
                        .child(div().w(px(32.))),
                )
                .children(p.routes.iter().enumerate().map(|(ix, r)| {
                    let project = p.clone();
                    h_flex()
                        .py_2()
                        .gap_3()
                        .border_b_1()
                        .border_color(cx.theme().border.opacity(0.6))
                        .child(div().flex_1().child({
                            let host = r.hostname(&p.slug, &snapshot.settings.domain_suffix);
                            let url = view
                                .status
                                .urls
                                .get(ix)
                                .cloned()
                                .unwrap_or_else(|| address_url(snapshot, &host));
                            link(format!("route-{ix}"), host, url, cx)
                        }))
                        .child(div().flex_1().child({
                            let to = match &r.target {
                                Target::Container => "Container (shared network)".to_string(),
                                Target::HostPort { address } => address.clone(),
                            };
                            if r.backend == zevo_core::model::Backend::Https {
                                format!("{to} · HTTPS")
                            } else {
                                to
                            }
                        }))
                        .child(mono(r.port.to_string(), cx).w(px(70.)))
                        .child(
                            mono(r.path_prefix.clone().unwrap_or_else(|| "/".into()), cx)
                                .w(px(90.)),
                        )
                        .child({
                            let (color, label) = match view.status.backends.get(ix) {
                                Some(Listening::Yes) => (cx.theme().success, "Listening"),
                                Some(Listening::LoopbackOnly) => (cx.theme().warning, "Only on 127.0.0.1"),
                                Some(Listening::No) => (cx.theme().muted_foreground, "Not listening"),
                                _ => (cx.theme().muted_foreground, "Not checked"),
                            };
                            h_flex()
                                .w(px(150.))
                                .gap_1p5()
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(div().text_sm().child(label))
                        })
                        .child(div().w(px(32.)).when(!p.declared, |cell| {
                            cell.child(
                                Button::new(("remove-route", ix))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Close)
                                    .tooltip("Remove this route")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_route(project.clone(), ix, window, cx)
                                    })),
                            )
                        }))
                }))
                .when(p.routes.is_empty(), |list| {
                    list.child(
                        div()
                            .py_4()
                            .text_color(cx.theme().muted_foreground)
                            .child("No routes yet."),
                    )
                })
                .child(if p.declared {
                    div().pt_3().text_color(cx.theme().muted_foreground).child(
                        "These routes are defined in the project's devcontainer.json, under \
                         customizations.zevo. Edit them there; Zevo picks up changes within a few seconds.",
                    )
                } else {
                    h_flex().pt_3().child({
                        let project = p.clone();
                        Button::new("add-route")
                            .small()
                            .icon(IconName::Plus)
                            .label("Add route")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.ask_new_route(project.clone(), window, cx)
                            }))
                    })
                }),
            LOGS_TAB => {
                let slug = p.slug.clone();
                let shown = self.logs.as_ref().filter(|(for_slug, _)| for_slug == &p.slug);
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(caption("Container output", cx))
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("Last {LOG_LINES} lines, refreshed every few seconds.")),
                            )
                            .child(
                                Button::new("refresh-logs")
                                    .ghost()
                                    .small()
                                    .icon(IconName::RefreshCw)
                                    .tooltip("Refresh")
                                    .loading(self.logs_loading)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.load_logs(slug.clone(), cx)
                                    })),
                            ),
                    )
                    .child(match shown {
                        None => div().text_color(cx.theme().muted_foreground).child("Loading…"),
                        Some((_, Err(e))) => div().text_color(cx.theme().muted_foreground).child(e.clone()),
                        Some((_, Ok(text))) if text.trim().is_empty() => div()
                            .text_color(cx.theme().muted_foreground)
                            .child("The container has not printed anything."),
                        Some((_, Ok(text))) => v_flex()
                            .p_3()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .border_1()
                            .border_color(cx.theme().border)
                            // One element per line; an empty line still takes a row.
                            .children(text.lines().map(|line| {
                                mono(if line.is_empty() { " ".to_string() } else { line.to_string() }, cx)
                            })),
                    })
            }
            _ => v_flex()
                .gap_2()
                .child(
                    h_flex().gap_2().child(caption("Traefik file", cx)).child(
                        mono(zevo_core::traefik::file_name(&p.slug), cx)
                            .text_color(cx.theme().muted_foreground),
                    ),
                )
                .child(
                    v_flex()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .border_1()
                        .border_color(cx.theme().border)
                        // One element per line; leading spaces are kept as non-breaking.
                        .children(view.yaml.lines().map(|line| {
                            let indent = line.len() - line.trim_start().len();
                            mono(line.trim_start().to_string(), cx)
                                .pl(px(indent as f32 * 7.8))
                                .when(line.starts_with('#'), |d| {
                                    d.text_color(cx.theme().muted_foreground)
                                })
                        })),
                ),
        };

        v_flex()
            .size_full()
            .child(header)
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(tabs),
            )
            .child(
                div().flex_1().overflow_hidden().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_4().child(body)),
                ),
            )
    }

    fn render_traefik(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        let adopted = snapshot
            .settings
            .traefik
            .as_ref()
            .and_then(|t| t.container.clone());
        let header = self.page_header(
            "Traefik",
            h_flex().when_some(adopted, |row, container| {
                row.child(
                    Button::new("detect-again")
                        .small()
                        .icon(IconName::RefreshCw)
                        .label("Detect again")
                        .tooltip("Re-read this Traefik's entrypoint, HTTPS and network settings")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run_setup(Some(container.clone()), cx)
                        })),
                )
            }),
            cx,
        );
        let details = match &snapshot.settings.traefik {
            None => v_flex().child(Alert::warning(
                "setup",
                "Traefik is not set up yet. Run `zevo setup` in a terminal, then refresh.",
            )),
            Some(t) => v_flex()
                .gap_2()
                .child(field(
                    "Container",
                    mono(
                        t.container
                            .clone()
                            .unwrap_or_else(|| "not in a container".into()),
                        cx,
                    ),
                    cx,
                ))
                .child(field(
                    "Routes folder",
                    mono(t.dynamic_dir.display().to_string(), cx),
                    cx,
                ))
                .child(field(
                    "Entrypoint",
                    mono(
                        format!(
                            "{} · port {}{}",
                            t.entrypoint,
                            t.http_port,
                            if t.tls { " · HTTPS" } else { "" }
                        ),
                        cx,
                    ),
                    cx,
                ))
                .child(field(
                    "Network",
                    mono(snapshot.settings.network.clone(), cx),
                    cx,
                )),
        };
        let routers = v_flex()
            .child(
                h_flex()
                    .py_2()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(caption("Address", cx).flex_1())
                    .child(caption("Owner", cx).w(px(200.)))
                    .child(caption("File", cx).flex_1()),
            )
            .children(snapshot.routers.iter().enumerate().map(|(ix, r)| {
                let owner = match &r.slug {
                    Some(slug) => {
                        let target = slug.clone();
                        div().child(
                            div()
                                .id(("owner", ix))
                                .cursor_pointer()
                                .text_color(cx.theme().link)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.go(Page::Project(target.clone()), cx)
                                }))
                                .child(slug.clone()),
                        )
                    }
                    None => div()
                        .text_color(cx.theme().muted_foreground)
                        .child("Not managed by Zevo"),
                };
                h_flex()
                    .py_2()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .child(v_flex().flex_1().gap_0p5().children(
                        r.hostnames.iter().enumerate().map(|(host_ix, host)| {
                            link(
                                format!("router-{ix}-{host_ix}"),
                                host.clone(),
                                address_url(snapshot, host),
                                cx,
                            )
                        }),
                    ))
                    .child(owner.w(px(200.)))
                    .child(
                        mono(
                            r.file
                                .file_name()
                                .map(|f| f.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            cx,
                        )
                        .flex_1()
                        .text_color(cx.theme().muted_foreground),
                    )
            }));
        v_flex().size_full().child(header).child(
            div().flex_1().overflow_hidden().child(
                v_flex().size_full().overflow_y_scrollbar().child(
                    v_flex().p_4().gap_5().child(details).child(
                        v_flex()
                            .gap_1()
                            .child(caption("Routers", cx))
                            .child(routers),
                    ),
                ),
            ),
        )
    }

    fn render_settings(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        v_flex().size_full().child(self.page_header("Settings", div(), cx)).child(
            v_flex()
                .p_4()
                .gap_2()
                .child(field("Domain suffix", mono(snapshot.settings.domain_suffix.clone(), cx), cx))
                .child(field("Network", mono(snapshot.settings.network.clone(), cx), cx))
                .child(field(
                    "Runtime",
                    match &snapshot.runtime {
                        Ok(version) => version.clone(),
                        Err(e) => format!("Unreachable: {e}"),
                    },
                    cx,
                ))
                .child(field(
                    "Editor",
                    if snapshot.editors.is_empty() {
                        div().text_color(cx.theme().muted_foreground).child(
                            "No supported editor found (VS Code, Cursor, Windsurf, VSCodium or Zed).",
                        )
                    } else {
                        let current = snapshot.editor();
                        h_flex().gap_2().children(snapshot.editors.iter().enumerate().map(|(ix, editor)| {
                            let editor = *editor;
                            Button::new(("editor", ix))
                                .small()
                                .label(editor.name())
                                .when(current == Some(editor), |b| b.primary())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    match this.backend.set_editor(editor) {
                                        Ok(()) => this.reload_now(cx),
                                        Err(e) => window.push_notification(format!("Could not save: {e}"), cx),
                                    }
                                }))
                        }))
                    },
                    cx,
                ))
                .child(
                    div()
                        .pt_3()
                        .text_color(cx.theme().muted_foreground)
                        .child("Settings are stored in settings.toml in the Zevo config folder and can be edited there."),
                ),
        )
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = TitleBar::new().child(
            h_flex()
                .gap_2()
                .text_sm()
                .child("Captain Zevo")
                .when(self.demo, |row| {
                    row.child(Tag::info().small().child("Example data"))
                }),
        );

        let Some(snapshot) = self.snapshot.clone() else {
            let content = match &self.error {
                Some(e) => {
                    div().child(Alert::error("load", e.clone()).title("Could not load projects"))
                }
                None => div().child(
                    v_flex()
                        .gap_4()
                        .items_center()
                        .child(img(self.logo.clone()).size(px(112.)))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Spinner::new())
                                .child("Loading projects…"),
                        ),
                ),
            };
            return v_flex()
                .size_full()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(title_bar)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(content),
                );
        };

        let page = match self.page.clone() {
            _ if snapshot.needs_setup() => self.render_setup(&snapshot, cx),
            Page::Add => self.render_add(&snapshot, cx),
            Page::Projects(filter) => self.render_projects(&snapshot, &filter, cx),
            Page::Project(slug) => {
                match snapshot.projects.iter().find(|p| p.project.slug == slug) {
                    Some(view) => self.render_project(&snapshot, view, cx),
                    // Removed elsewhere while it was open.
                    None => self.render_projects(&snapshot, &None, cx),
                }
            }
            Page::Traefik => self.render_traefik(&snapshot, cx),
            Page::Settings => self.render_settings(&snapshot, cx),
            Page::Hosts => self.render_hosts(&snapshot, cx),
        };

        let banners = v_flex()
            .flex_shrink_0()
            .when_some(self.error.clone(), |col, e| {
                col.child(Alert::error("refresh-error", e).banner())
            })
            .children(
                snapshot
                    .warnings
                    .iter()
                    .enumerate()
                    .map(|(ix, w)| Alert::warning(("warning", ix), w.clone()).banner()),
            );

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.reload(cx)))
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .items_start()
                    .overflow_hidden()
                    .child(
                        div()
                            .relative()
                            .h_full()
                            .flex_shrink_0()
                            .overflow_hidden()
                            .child(self.render_sidebar(&snapshot, cx))
                            // A large, faint, tilted logo in the empty lower
                            // part of the sidebar, cropped by its edge. It has
                            // no mouse handlers, so clicks pass through it.
                            .child(
                                svg()
                                    .data(LOGO_MONO)
                                    .absolute()
                                    .left(px(-58.))
                                    .bottom(px(34.))
                                    .size(px(300.))
                                    .text_color(cx.theme().foreground.opacity(0.045))
                                    .with_transformation(Transformation::rotate(radians(-0.26))),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(banners)
                            .child(div().flex_1().overflow_hidden().child(page)),
                    ),
            )
    }
}
