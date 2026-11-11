//! First-run setup: choose the Traefik that Zevo will write routes for.

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zevo_core::setup::Candidate;

use super::{Shell, field, mono};
use crate::backend::Snapshot;

#[derive(Default)]
pub(super) struct SetupState {
    /// `None` until the machine has been searched.
    candidates: Option<Result<Vec<Candidate>, String>>,
    searching: bool,
    busy: bool,
    error: Option<String>,
}

impl Shell {
    /// Looks for Traefik containers, once, when setup is needed.
    pub(super) fn find_traefik(&mut self, cx: &mut Context<Self>) {
        if self.setup.searching {
            return;
        }
        self.setup.searching = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.traefik_candidates() })
                .await;
            _ = this.update(cx, |this, cx| {
                this.setup.searching = false;
                this.setup.candidates = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn setup_searched(&self) -> bool {
        self.setup.candidates.is_some() || self.setup.searching
    }

    /// Adopts the named container, or starts a managed Traefik when `None`.
    pub(super) fn run_setup(&mut self, adopt: Option<String>, cx: &mut Context<Self>) {
        self.setup.busy = true;
        self.setup.error = None;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match adopt {
                        Some(container) => backend.adopt(&container),
                        None => backend.bootstrap(),
                    }
                })
                .await;
            _ = this.update(cx, |this, cx| {
                this.setup.busy = false;
                match result {
                    Ok(()) => {
                        this.setup = SetupState::default();
                        this.reload_now(cx);
                    }
                    Err(e) => {
                        // Also shown as a banner, for when setup is re-run from the Traefik page.
                        this.error = Some(e.clone());
                        this.setup.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_setup(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        let header = self.page_header("Set up Captain Zevo", div(), cx);
        let intro = div().max_w(px(620.)).child(
            "Zevo gives each project an address by writing route files for Traefik. \
             Choose the Traefik it should use on this machine.",
        );

        let body = match (&snapshot.runtime, &self.setup.candidates) {
            (Err(e), _) => v_flex().gap_3().child(
                Alert::error(
                    "no-runtime",
                    format!(
                        "Start Docker (or another container runtime), then refresh. Details: {e}"
                    ),
                )
                .title("No container runtime found"),
            ),
            (_, None) => v_flex().child(
                h_flex()
                    .gap_2()
                    .child(Spinner::new())
                    .child("Looking for Traefik…"),
            ),
            (_, Some(Err(e))) => v_flex().child(
                Alert::error("search-failed", e.clone()).title("Could not look for Traefik"),
            ),
            (_, Some(Ok(candidates))) => {
                let found = candidates.iter().enumerate().map(|(ix, c)| {
                    let name = c.container.clone();
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("Use my Traefik"))
                                .child(mono(c.container.clone(), cx).text_color(cx.theme().muted_foreground)),
                        )
                        .when(c.adoptable(), |card| {
                            card.child(div().text_color(cx.theme().muted_foreground).child(
                                "Zevo adds its own route files to this folder. Your existing routes stay as they are.",
                            ))
                            .when_some(c.dynamic_dir.clone(), |card, dir| {
                                card.child(field("Routes folder", mono(dir.display().to_string(), cx), cx))
                            })
                            .when_some(c.network.clone(), |card, network| {
                                card.child(field("Network", mono(network, cx), cx))
                            })
                            .when_some(c.entrypoint.clone().zip(c.http_port), |card, (name, port)| {
                                card.child(field("Entrypoint", mono(format!("{name} · port {port}{}", if c.tls { " · HTTPS" } else { "" }), cx), cx))
                            })
                            .child(h_flex().child(
                                Button::new(("adopt", ix))
                                    .primary()
                                    .label("Use this Traefik")
                                    .loading(self.setup.busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.run_setup(Some(name.clone()), cx)
                                    })),
                            ))
                        })
                        .when(!c.adoptable(), |card| {
                            card.child("Zevo cannot use this Traefik as it is:")
                                .children(c.problems.iter().map(|p| div().pl_3().child(format!("• {p}"))))
                        })
                });
                let other_running = candidates.iter().any(|c| c.running);
                v_flex().gap_3().children(found).child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(if candidates.is_empty() {
                            "Start Traefik"
                        } else {
                            "Start a separate Traefik"
                        }))
                        .child(div().text_color(cx.theme().muted_foreground).child(if other_running {
                            "Zevo runs its own Traefik container (zevo-traefik) next to yours. Port 80 is probably taken, so addresses will include a port such as :8080."
                        } else {
                            "No Traefik was found. Zevo starts its own container (zevo-traefik) and keeps it running across restarts."
                        }))
                        .child(h_flex().child(
                            Button::new("bootstrap")
                                .label("Start Traefik")
                                .when(candidates.iter().all(|c| !c.adoptable()), |b| b.primary())
                                .loading(self.setup.busy)
                                .on_click(cx.listener(|this, _, _, cx| this.run_setup(None, cx))),
                        )),
                )
            }
        };

        v_flex().size_full().child(header).child(
            v_flex()
                .p_4()
                .gap_4()
                .max_w(px(760.))
                .child(intro)
                .when_some(self.setup.error.clone(), |col, e| {
                    col.child(Alert::error("setup-error", e).title("Setup did not finish"))
                })
                .child(body),
        )
    }
}
