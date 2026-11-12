//! Adding a project from a folder, and editing a project's routes.

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zevo_core::model::{Project, Route, Target};

use super::{Page, Shell, caption, field, mono};
use crate::backend::Snapshot;

struct RouteRow {
    subdomain: Entity<InputState>,
    port: Entity<InputState>,
}

/// State of the "Add project" page.
pub(super) struct AddForm {
    /// Host the folder is on; `local` for this machine.
    host: String,
    folder: Entity<InputState>,
    /// Set once the folder has been inspected.
    draft: Option<Project>,
    name: Entity<InputState>,
    slug: Entity<InputState>,
    rows: Vec<RouteRow>,
    error: Option<String>,
    busy: bool,
    /// "Start a new project": its name, where to create it, and the starter.
    new_name: Entity<InputState>,
    new_parent: Entity<InputState>,
    new_template: &'static str,
    /// Ids of the extras that are ticked.
    new_extras: Vec<&'static str>,
    creating: bool,
    _folder_enter: Subscription,
}

fn text_input(
    placeholder: &'static str,
    value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn route_row(route: Option<&Route>, window: &mut Window, cx: &mut App) -> RouteRow {
    RouteRow {
        subdomain: text_input(
            "none",
            route.and_then(|r| r.subdomain.as_deref()).unwrap_or(""),
            window,
            cx,
        ),
        port: text_input(
            "port",
            &route.map(|r| r.port.to_string()).unwrap_or_default(),
            window,
            cx,
        ),
    }
}

/// Reads a route from a subdomain and port typed by the user.
fn parse_route(subdomain: &str, port: &str) -> Result<Route, String> {
    let subdomain = subdomain.trim().trim_end_matches('.').to_lowercase();
    let port: u16 = port
        .trim()
        .parse()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| format!("'{}' is not a port number (1 to 65535)", port.trim()))?;
    Ok(Route {
        backend: zevo_core::model::Backend::default(),
        host: None,
        subdomain: (!subdomain.is_empty()).then_some(subdomain),
        port,
        path_prefix: None,
        target: Target::Container,
    })
}

impl Shell {
    /// Opens the add page for a known folder and reads it straight away.
    pub(super) fn open_add_for(
        &mut self,
        host: String,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_add(window, cx);
        if let Some(form) = &mut self.add {
            form.host = host;
            form.folder
                .update(cx, |input, cx| input.set_value(folder, window, cx));
        }
        self.inspect(window, cx);
    }

    pub(super) fn open_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // New projects default to sitting beside an existing local one.
        let default_parent = self
            .snapshot
            .as_ref()
            .and_then(|s| {
                s.projects
                    .iter()
                    .find(|p| p.project.host == zevo_core::model::LOCAL_HOST_ID)
                    .and_then(|p| {
                        p.project
                            .path
                            .parent()
                            .map(|d| d.to_string_lossy().into_owned())
                    })
            })
            .unwrap_or_else(|| "~".to_string());
        let folder = text_input("/path/to/project", "", window, cx);
        // Enter in the folder box continues, like the button.
        let _folder_enter = cx.subscribe_in(
            &folder,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.inspect(window, cx);
                }
            },
        );
        self.add = Some(AddForm {
            host: zevo_core::model::LOCAL_HOST_ID.to_string(),
            folder,
            draft: None,
            name: text_input("Name", "", window, cx),
            slug: text_input("short-name", "", window, cx),
            rows: Vec::new(),
            error: None,
            busy: false,
            new_name: text_input("My app", "", window, cx),
            new_parent: text_input("/path/to/projects", &default_parent, window, cx),
            new_template: zevo_core::templates::TEMPLATES[0].id,
            new_extras: zevo_core::templates::EXTRAS
                .iter()
                .filter(|e| e.default)
                .map(|e| e.id)
                .collect(),
            creating: false,
            _folder_enter,
        });
        self.go(Page::Add, cx);
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose project folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                if let Some(form) = &this.add {
                    form.folder.update(cx, |input, cx| {
                        input.set_value(path.to_string_lossy().into_owned(), window, cx)
                    });
                }
                this.inspect(window, cx);
            });
        })
        .detach();
    }

    /// Reads the chosen folder and fills in the rest of the form.
    fn inspect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.add else { return };
        let folder = form.folder.read(cx).value().to_string();
        let host = form.host.clone();
        form.busy = true;
        form.error = None;
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.draft(&host, &folder) })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.add else { return };
                form.busy = false;
                match result {
                    Ok(draft) => {
                        form.name
                            .update(cx, |i, cx| i.set_value(draft.name.clone(), window, cx));
                        form.slug
                            .update(cx, |i, cx| i.set_value(draft.slug.clone(), window, cx));
                        form.rows = draft
                            .routes
                            .iter()
                            .map(|r| route_row(Some(r), window, cx))
                            .collect();
                        form.draft = Some(draft);
                    }
                    Err(e) => form.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Creates the new project's folder, then reads it like any other.
    fn create_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.add else { return };
        let name = form.new_name.read(cx).value().to_string();
        let parent = form.new_parent.read(cx).value().to_string();
        let template = form.new_template;
        let extras = form.new_extras.clone();
        form.creating = true;
        form.error = None;
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    backend.create_project(&parent, &name, template, &extras)
                })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.add else { return };
                form.creating = false;
                match result {
                    Ok(folder) => {
                        form.folder
                            .update(cx, |input, cx| input.set_value(folder, window, cx));
                        this.inspect(window, cx);
                    }
                    Err(e) => form.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn submit_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.add else { return };
        let Some(draft) = &form.draft else { return };
        let mut project = draft.clone();
        project.name = form.name.read(cx).value().trim().to_string();
        project.slug = form.slug.read(cx).value().trim().to_string();
        let declared = project.declared;
        let routes: Result<Vec<Route>, String> = if declared {
            Ok(project.routes.clone())
        } else {
            form.rows
                .iter()
                .map(|row| parse_route(&row.subdomain.read(cx).value(), &row.port.read(cx).value()))
                .collect()
        };
        match routes {
            Ok(routes) => project.routes = routes,
            Err(e) => {
                form.error = Some(e);
                cx.notify();
                return;
            }
        }
        form.busy = true;
        form.error = None;
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let slug = project.slug.clone();
            let result = cx
                .background_spawn(async move { backend.add_project(&project) })
                .await;
            _ = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.add = None;
                    window.push_notification(format!("Added {slug}"), cx);
                    this.go(Page::Project(slug), cx);
                    this.reload(cx);
                }
                Err(e) => {
                    if let Some(form) = &mut this.add {
                        form.busy = false;
                        form.error = Some(e);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_add(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        let header = self.page_header(
            h_flex()
                .gap_2()
                .child(
                    Button::new("add-back")
                        .ghost()
                        .small()
                        .icon(IconName::ArrowLeft)
                        .tooltip("All projects")
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Projects(None), cx))),
                )
                .child(match self.add.as_ref().map(|f| f.host.as_str()) {
                    Some(host) if host != zevo_core::model::LOCAL_HOST_ID => {
                        format!("Add project on {host}")
                    }
                    _ => "Add project".to_string(),
                }),
            div(),
            cx,
        );
        let Some(form) = &self.add else {
            return v_flex().size_full().child(header);
        };
        let suffix = &snapshot.settings.domain_suffix;
        let slug = form.slug.read(cx).value().trim().to_string();

        let folder = v_flex()
            .gap_2()
            .child(caption("Folder", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&form.folder)))
                    // The folder picker only sees this machine.
                    .when(form.host == zevo_core::model::LOCAL_HOST_ID, |row| {
                        row.child(
                            Button::new("browse")
                                .label("Browse…")
                                .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                        )
                    })
                    .child(
                        Button::new("inspect")
                            .label(if form.draft.is_some() { "Read again" } else { "Continue" })
                            .when(form.draft.is_none(), |b| b.primary())
                            .loading(form.busy && form.draft.is_none())
                            .on_click(cx.listener(|this, _, window, cx| this.inspect(window, cx))),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if form.host == zevo_core::model::LOCAL_HOST_ID { "The project folder on this machine. Zevo reads its devcontainer settings and changes nothing in it." } else { "The full path of the project folder on that machine. Zevo reads its devcontainer settings over SSH." }),
            );

        let starting_fresh = form.draft.is_none() && form.host == zevo_core::model::LOCAL_HOST_ID;
        let new_project = starting_fresh.then(|| {
            v_flex()
                .gap_3()
                .pt_4()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(caption("Or start a new project", cx))
                .child(
                    h_flex()
                        .gap_3()
                        .items_end()
                        .child(
                            v_flex()
                                .gap_1()
                                .w(px(220.))
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child("Name"))
                                .child(Input::new(&form.new_name)),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .flex_1()
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child("Create it in"))
                                .child(Input::new(&form.new_parent)),
                        ),
                )
                .child(h_flex().gap_2().flex_wrap().children(
                    zevo_core::templates::TEMPLATES.iter().enumerate().map(|(ix, template)| {
                        let id = template.id;
                        Button::new(("template", ix))
                            .small()
                            .label(template.name)
                            .tooltip(template.description)
                            .when(form.new_template == id, |b| b.primary())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = &mut this.add {
                                    form.new_template = id;
                                }
                                cx.notify();
                            }))
                    }),
                ))
                .child(
                    v_flex()
                        .gap_1p5()
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child("Include"))
                        .child(h_flex().gap_x_4().gap_y_1p5().flex_wrap().children(
                            zevo_core::templates::EXTRAS.iter().enumerate().map(|(ix, extra)| {
                                let id = extra.id;
                                Checkbox::new(("extra", ix))
                                    .label(extra.name)
                                    .tooltip(extra.description)
                                    .checked(form.new_extras.contains(&id))
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        if let Some(form) = &mut this.add {
                                            form.new_extras.retain(|e| *e != id);
                                            if *checked {
                                                form.new_extras.push(id);
                                            }
                                        }
                                        cx.notify();
                                    }))
                            }),
                        )),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            Button::new("create-new")
                                .label("Create project")
                                .loading(form.creating)
                                .on_click(cx.listener(|this, _, window, cx| this.create_new(window, cx))),
                        )
                        .child(div().flex_1().text_sm().text_color(cx.theme().muted_foreground).child(
                            "Creates a folder with a devcontainer.json for that stack. You review it before it is added.",
                        )),
                )
        });

        let details = form.draft.as_ref().map(|draft| {
            let rows = form.rows.iter().enumerate().map(|(ix, row)| {
                let sub = row.subdomain.read(cx).value().trim().trim_end_matches('.').to_lowercase();
                let host = if sub.is_empty() {
                    format!("{slug}.{suffix}")
                } else {
                    format!("{sub}.{slug}.{suffix}")
                };
                h_flex()
                    .gap_2()
                    .child(div().w(px(150.)).child(Input::new(&row.subdomain)))
                    .child(div().w(px(100.)).child(Input::new(&row.port)))
                    .child(mono(format!("http://{host}"), cx).flex_1().text_color(cx.theme().muted_foreground))
                    .child(
                        Button::new(("remove-row", ix))
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip("Remove this route")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = &mut this.add {
                                    form.rows.remove(ix);
                                }
                                cx.notify();
                            })),
                    )
            });
            v_flex()
                .gap_5()
                .child(
                    v_flex()
                        .gap_2()
                        .child(caption("Found", cx))
                        .child(field("Devcontainer", format!("{:?}", draft.kind).to_lowercase(), cx))
                        .when_some(draft.service.clone(), |col, service| {
                            col.child(field("Service", mono(service, cx), cx))
                        }),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(caption("Project", cx))
                        .child(field("Name", div().max_w(px(360.)).child(Input::new(&form.name)), cx))
                        .child(field("Short name", div().max_w(px(360.)).child(Input::new(&form.slug)), cx))
                        .child(field(
                            "",
                            div().text_sm().text_color(cx.theme().muted_foreground).child(
                                "Used in the address. Lowercase letters, digits and dashes.",
                            ),
                            cx,
                        )),
                )
                .child(if draft.declared {
                    v_flex()
                        .gap_2()
                        .child(caption("Addresses", cx))
                        .children(draft.routes.iter().map(|r| {
                            let mut r = r.clone();
                            r.target = zevo_core::model::Target::Container;
                            h_flex()
                                .gap_3()
                                .child(mono(format!("{}{}", r.hostname(&slug, suffix), r.path_prefix.as_deref().unwrap_or("")), cx))
                                .child(mono(format!("→ {}", r.port), cx).text_color(cx.theme().muted_foreground))
                        }))
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                            "Defined in this project's devcontainer.json (customizations.zevo). Zevo follows that file.",
                        ))
                } else {
                    v_flex()
                        .gap_2()
                        .child(caption("Addresses", cx))
                        .when(!form.rows.is_empty(), |col| {
                            col.child(
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(div().w(px(150.)).child("Subdomain (optional)"))
                                    .child(div().w(px(100.)).child("Port")),
                            )
                        })
                        .children(rows)
                        .when(form.rows.is_empty(), |col| {
                            col.child(div().text_color(cx.theme().muted_foreground).child(
                                "No ports were found in the devcontainer settings. Add a route to give the project an address.",
                            ))
                        })
                        .child(h_flex().child(
                            Button::new("add-row").small().icon(IconName::Plus).label("Add route").on_click(
                                cx.listener(|this, _, window, cx| {
                                    let row = route_row(None, window, cx);
                                    if let Some(form) = &mut this.add {
                                        form.rows.push(row);
                                    }
                                    cx.notify();
                                }),
                            ),
                        ))
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("submit-add")
                                .primary()
                                .label("Add project")
                                .loading(form.busy)
                                .on_click(cx.listener(|this, _, window, cx| this.submit_add(window, cx))),
                        )
                        .child(
                            Button::new("cancel-add")
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Projects(None), cx))),
                        ),
                )
        });

        v_flex().size_full().child(header).child(
            div().flex_1().overflow_hidden().child(
                v_flex().size_full().overflow_y_scrollbar().child(
                    v_flex()
                        .p_4()
                        .gap_5()
                        .max_w(px(760.))
                        .child(folder)
                        .when_some(form.error.clone(), |col, e| {
                            col.child(Alert::error("add-error", e))
                        })
                        .children(new_project)
                        .children(details),
                ),
            ),
        )
    }

    /// Saves a changed copy of a project, then refreshes.
    fn save_routes(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        match self.backend.save_project(&project) {
            Ok(()) => self.reload_now(cx),
            Err(e) => window.push_notification(format!("Could not save: {e}"), cx),
        }
    }

    pub(super) fn remove_route(
        &mut self,
        mut project: Project,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix < project.routes.len() {
            project.routes.remove(ix);
            self.save_routes(project, window, cx);
        }
    }

    /// Asks for a subdomain and port, then adds the route to the project.
    pub(super) fn ask_new_route(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subdomain = text_input("none", "", window, cx);
        let port = text_input("3000", "", window, cx);
        let shell = cx.entity().downgrade();
        let focus_port = port.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (shell, project) = (shell.clone(), project.clone());
            let (subdomain_value, port_value) = (subdomain.clone(), port.clone());
            dialog
                .title(format!("Add a route to {}", project.name))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_1()
                                .child("Port the app listens on")
                                .child(Input::new(&port)),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child("Subdomain (optional)")
                                .child(Input::new(&subdomain))
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                                    format!("Leave empty for {}.localhost; \"api\" gives api.{}.localhost.", project.slug, project.slug),
                                )),
                        ),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(DialogAction::new().child(Button::new("confirm-route").primary().label("Add route"))),
                )
                .on_ok(move |_, window, cx| {
                    let parsed = parse_route(&subdomain_value.read(cx).value(), &port_value.read(cx).value());
                    match parsed {
                        Ok(route) => {
                            let mut project = project.clone();
                            project.routes.push(route);
                            _ = shell.update(cx, |this, cx| this.save_routes(project, window, cx));
                            true
                        }
                        Err(e) => {
                            window.push_notification(e, cx);
                            false
                        }
                    }
                })
        });
        focus_port.update(cx, |input, cx| input.focus(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_route;

    #[test]
    fn parses_typed_routes() {
        let r = parse_route("", " 3000 ").unwrap();
        assert_eq!((r.subdomain, r.port), (None, 3000));
        let r = parse_route(" API. ", "8080").unwrap();
        assert_eq!(r.subdomain.as_deref(), Some("api"));
        assert!(parse_route("", "0").is_err());
        assert!(parse_route("", "http").is_err());
        assert!(parse_route("", "70000").is_err());
    }
}
