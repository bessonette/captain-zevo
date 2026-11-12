//! Remote hosts: machines reached over SSH that run project containers.

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Page, Shell, caption, mono};
use crate::backend::Snapshot;

/// The "add a host" form.
pub(super) struct HostForm {
    ssh: Entity<InputState>,
    name: Entity<InputState>,
    address: Entity<InputState>,
    docker_socket: Entity<InputState>,
    docker_command: Entity<InputState>,
    error: Option<String>,
    busy: bool,
}

impl Shell {
    pub(super) fn open_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_form.is_none() {
            let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
            };
            self.host_form = Some(HostForm {
                ssh: input("user@192.168.1.20", window, cx),
                name: input("optional", window, cx),
                address: input("optional", window, cx),
                docker_socket: input("found automatically", window, cx),
                docker_command: input("e.g. ~/tools/docker.sh", window, cx),
                error: None,
                busy: false,
            });
        }
        self.go(Page::Hosts, cx);
    }

    fn submit_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        let ssh = form.ssh.read(cx).value().to_string();
        let name = form.name.read(cx).value().to_string();
        let address = form.address.read(cx).value().to_string();
        let docker_socket = form.docker_socket.read(cx).value().to_string();
        let docker_command = form.docker_command.read(cx).value().to_string();
        form.busy = true;
        form.error = None;
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            // Connecting over SSH can take several seconds.
            let result = cx
                .background_spawn(async move {
                    backend.add_host(&ssh, &name, &address, &docker_socket, &docker_command)
                })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.host_form else {
                    return;
                };
                form.busy = false;
                match result {
                    Ok(()) => {
                        for input in [
                            &form.ssh,
                            &form.name,
                            &form.address,
                            &form.docker_socket,
                            &form.docker_command,
                        ] {
                            input.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        window.push_notification("Host added", cx);
                        this.reload_now(cx);
                    }
                    Err(e) => form.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_host(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        match self.backend.remove_host(&id) {
            Ok(note) => {
                if let Some(note) = note {
                    window.push_notification(note, cx);
                }
                self.reload_now(cx)
            }
            Err(e) => window.push_notification(format!("Could not remove {id}: {e}"), cx),
        }
    }

    pub(super) fn render_hosts(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        let header = self.page_header("Hosts", div(), cx);

        let table = v_flex()
            .child(
                h_flex()
                    .py_2()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(caption("Host", cx).w(px(170.)))
                    .child(caption("SSH", cx).w(px(210.)))
                    .child(caption("Docker", cx).flex_1())
                    .child(caption("Projects", cx).w(px(80.)))
                    .child(div().w(px(32.))),
            )
            .child(
                h_flex()
                    .py_2()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .child(
                        div()
                            .w(px(170.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("This machine"),
                    )
                    .child(
                        div()
                            .w(px(210.))
                            .text_color(cx.theme().muted_foreground)
                            .child("—"),
                    )
                    .child(div().flex_1().child(match &snapshot.runtime {
                        Ok(version) => version.clone(),
                        Err(e) => format!("Unreachable: {e}"),
                    }))
                    .child(
                        div().w(px(80.)).child(
                            snapshot
                                .projects
                                .iter()
                                .filter(|p| p.project.host == zevo_core::model::LOCAL_HOST_ID)
                                .count()
                                .to_string(),
                        ),
                    )
                    .child(div().w(px(32.))),
            )
            .children(snapshot.hosts.iter().enumerate().map(|(ix, view)| {
                let id = view.host.id.clone();
                let projects = snapshot
                    .projects
                    .iter()
                    .filter(|p| p.project.host == id)
                    .count();
                let (color, text) = match &view.status {
                    Ok(version) => (cx.theme().success, version.clone()),
                    Err(e) => (cx.theme().danger, e.clone()),
                };
                h_flex()
                    .py_2()
                    .gap_3()
                    .items_start()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .child(
                        v_flex()
                            .w(px(170.))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(view.host.name.clone()),
                            )
                            .child(
                                mono(view.host.address.clone(), cx)
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .child(mono(view.host.ssh.clone(), cx).w(px(210.)))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_start()
                            .child(
                                div()
                                    .mt(px(6.))
                                    .flex_shrink_0()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(color),
                            )
                            .child(div().min_w_0().child(text)),
                    )
                    .child(div().w(px(80.)).child(projects.to_string()))
                    .child(div().w(px(32.)).when(projects == 0, |cell| {
                        cell.child(
                            Button::new(("remove-host", ix))
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .tooltip("Remove this host")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_host(id.clone(), window, cx)
                                })),
                        )
                    }))
            }));

        let form = self.host_form.as_ref().map(|form| {
            let labelled = |label: &'static str, input: &Entity<InputState>, width: f32| {
                v_flex()
                    .gap_1()
                    .w(px(width))
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label))
                    .child(Input::new(input))
            };
            v_flex()
                .gap_3()
                .child(caption("Add a host", cx))
                .child(div().max_w(px(640.)).text_color(cx.theme().muted_foreground).child(
                    "A machine you can reach with SSH keys (Zevo cannot type a password). It needs Docker; \
                     Zevo starts one small container there, zevo-edge, to route to its projects. The address can be a name; Zevo follows it if the machine's IP changes.",
                ))
                .child(
                    h_flex()
                        .gap_3()
                        .items_end()
                        .child(labelled("SSH destination", &form.ssh, 230.))
                        .child(labelled("Name", &form.name, 150.))
                        .child(labelled("Address for routing", &form.address, 170.)),
                )
                .child(
                    v_flex()
                        .gap_1p5()
                        .child(div().max_w(px(640.)).text_sm().text_color(cx.theme().muted_foreground).child(
                            "Zevo looks for Docker's socket there. Fill in one of these only if it cannot be \
                             found, or if Docker is run through a wrapper (a script, or \"sudo -n docker\").",
                        ))
                        .child(
                            h_flex()
                                .gap_3()
                                .items_end()
                                .child(labelled("Docker socket", &form.docker_socket, 230.))
                                .child(labelled("or Docker command", &form.docker_command, 230.)),
                        ),
                )
                .child(h_flex().child(
                    Button::new("add-host")
                        .primary()
                        .label("Add host")
                        .loading(form.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.submit_host(window, cx))),
                ))
                .when_some(form.error.clone(), |col, e| {
                    col.child(Alert::error("host-error", e).title("Could not add the host"))
                })
        });

        v_flex().size_full().child(header).child(
            div().flex_1().overflow_hidden().child(
                v_flex()
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(v_flex().p_4().gap_6().child(table).children(form)),
            ),
        )
    }
}
