use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use super::connection_page::ConnectionPage;
use super::devices_page::DevicesPage;
use super::diagnostics_page::DiagnosticsPage;
use crate::app::state::{AppState, UiEvent};
use crate::session;

const PAGE_DEVICES: &str = "devices";
const PAGE_CONNECTED: &str = "connected";
const PAGE_DIAGNOSTICS: &str = "diagnostics";

pub fn page_for_state(state: session::State) -> &'static str {
    match state {
        session::State::Negotiating
        | session::State::Streaming
        | session::State::Recovering
        | session::State::Failed => PAGE_CONNECTED,
        _ => PAGE_DEVICES,
    }
}

pub fn run(state: Arc<AppState>) -> i32 {
    let app = adw::Application::new(
        Some(crate::config::APP_ID),
        gtk::gio::ApplicationFlags::empty(),
    );
    let ui_state = state.clone();
    app.connect_activate(move |app| {
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        build_window(app, &ui_state).present();
    });
    let about = gtk::gio::SimpleAction::new("about", None);
    let app_for_about = app.clone();
    about.connect_activate(move |_, _| show_about(&app_for_about));
    app.add_action(&about);

    app.run().into()
}

fn build_window(app: &adw::Application, state: &Arc<AppState>) -> adw::ApplicationWindow {
    let (cfg_w, cfg_h, cfg_max) = {
        let cfg = state.config.lock().expect("config lock");
        (
            cfg.general.window_width,
            cfg.general.window_height,
            cfg.general.window_maximized,
        )
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Wireless Display")
        .default_width(cfg_w)
        .default_height(cfg_h)
        .icon_name("phone-symbolic")
        .build();

    let toast_overlay = adw::ToastOverlay::new();

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Wireless Display", "");
    header.set_title_widget(Some(&title));

    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Menu")
        .build();
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Diagnostics"), Some("win.show-diagnostics"));
    menu.append(Some("About"), Some("app.about"));
    menu_button.set_menu_model(Some(&menu));
    header.pack_end(&menu_button);

    let fingerprint_short = state
        .metrics
        .snapshot()
        .get("net.fingerprint")
        .map(|v| match v {
            crate::diag::MetricValue::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .unwrap_or_else(|| "n/a".into());
    let port = state.config.lock().expect("config lock").network.listen_port;
    let address_line = format!("0.0.0.0:{port} — phones connect here");

    let devices_page = DevicesPage::build(
        "Advertising on your local network",
        &address_line,
        &fingerprint_short,
        {
            let weak_window = window.downgrade();
            let st = state.clone();
            move || {
                let code = st.start_pairing();
                match weak_window.upgrade() {
                    Some(win) => show_pairing_dialog(Some(&win), st.clone(), &code),
                    None => show_pairing_dialog::<adw::ApplicationWindow>(None, st.clone(), &code),
                }
            }
        },
    );
    devices_page.update_devices(&state.pairing.list_devices());

    let connection_page = ConnectionPage::build();
    let diagnostics_page = DiagnosticsPage::build(state, toast_overlay.clone());

    let stack = gtk::Stack::new();
    stack.add_titled(&devices_page.root, Some(PAGE_DEVICES), "Devices");
    stack.add_titled(&connection_page.root, Some(PAGE_CONNECTED), "Connected");
    stack.add_titled(
        &diagnostics_page.root,
        Some(PAGE_DIAGNOSTICS),
        "Diagnostics",
    );
    stack.set_visible_child_name(PAGE_DEVICES);

    let status_label = gtk::Label::builder()
        .label(state.session.state().to_string())
        .halign(gtk::Align::Start)
        .css_classes(["dimmed", "caption"])
        .build();
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(6);
    footer.set_margin_bottom(6);
    footer.append(&status_label);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&footer);
    toast_overlay.set_child(Some(&stack));
    toolbar.set_content(Some(&toast_overlay));

    window.set_content(Some(&toolbar));
    if cfg_max {
        window.maximize();
    }

    let diag_action = gtk::gio::SimpleAction::new("show-diagnostics", None);
    let weak_stack = stack.downgrade();
    diag_action.connect_activate(move |_, _| {
        if let Some(stack) = weak_stack.upgrade() {
            stack.set_visible_child_name(PAGE_DIAGNOSTICS);
        }
    });
    window.add_action(&diag_action);

    let close_state = state.clone();
    window.connect_close_request(move |win| {
        close_state.cancel_pairing();
        {
            let mut cfg = close_state.config.lock().expect("config lock");
            cfg.general.window_width = win.width().max(320);
            cfg.general.window_height = win.height().max(240);
            cfg.general.window_maximized = win.is_maximized();
            if let Err(e) = cfg.save() {
                tracing::warn!(error = %e, "could not persist window geometry");
            }
        }
        tracing::info!("window closed; exiting");
        glib::Propagation::Proceed
    });

    spawn_ui_bridge(
        state.clone(),
        window.clone(),
        stack,
        status_label,
        devices_page,
        connection_page,
        diagnostics_page,
        toast_overlay,
    );

    window
}

#[allow(clippy::too_many_arguments)]
fn spawn_ui_bridge(
    state: Arc<AppState>,
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    status_label: gtk::Label,
    devices_page: DevicesPage,
    connection_page: ConnectionPage,
    diagnostics_page: DiagnosticsPage,
    toast_overlay: adw::ToastOverlay,
) {
    glib::MainContext::default().spawn_local(async move {
        let rx = state.ui_receiver();
        while let Ok(event) = rx.recv().await {
            match event {
                UiEvent::State(s) => {
                    status_label.set_text(&s.to_string());
                    connection_page.update_state(s);
                    let target = page_for_state(s);
                    if stack.visible_child_name().map(|n| n.to_string()) != Some(target.into()) {
                        stack.set_visible_child_name(target);
                    }
                }
                UiEvent::Peer(peer) => connection_page.update_peer(peer.as_deref()),
                UiEvent::KnownDevices(devices) => devices_page.update_devices(&devices),
                UiEvent::PairingPrompt { code } => {
                    show_pairing_dialog(Some(&window), state.clone(), &code)
                }
                UiEvent::PairingOutcome { ok, message } => {
                    let toast = adw::Toast::new(&message);
                    if ok {
                        toast.set_timeout(4);
                    } else {
                        toast.set_timeout(6);
                    }
                    toast_overlay.add_toast(toast);
                }
                UiEvent::Metrics(snapshot) => {
                    connection_page.update_metrics(&snapshot);
                    diagnostics_page.update_metrics(&snapshot);
                }
            }
        }
    });
}

fn show_pairing_dialog<W: IsA<gtk::Widget> + 'static>(
    parent: Option<&W>,
    state: Arc<AppState>,
    code: &str,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Pair a phone")
        .body(format!(
            "On your phone, choose this computer and enter the code:\n\n{code}\n\nThe code expires in two minutes."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.set_response_appearance("cancel", adw::ResponseAppearance::Destructive);
    dialog.connect_response(None, move |_dlg, response| {
        if response == "cancel" {
            state.cancel_pairing();
        }
    });
    dialog.present(parent);
}

fn show_about(app: &adw::Application) {
    adw::AboutWindow::builder()
        .application(app)
        .application_name("Wireless Display")
        .application_icon("phone-symbolic")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Wireless Display contributors")
        .license_type(gtk::License::Unknown)
        .comments("Mirror your Android phone's screen and audio to this Linux desktop.")
        .website("https://example.org/wireless-display")
        .build()
        .present();
}
