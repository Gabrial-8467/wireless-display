use std::collections::BTreeMap;
use std::sync::Arc;

use adw::prelude::*;

use crate::app::state::AppState;
use crate::diag::{MetricValue, report_path};

pub struct DiagnosticsPage {
    pub root: gtk::ScrolledWindow,
    metrics_list: gtk::ListBox,
}

impl DiagnosticsPage {
    pub fn build(state: &Arc<AppState>, toast_overlay: adw::ToastOverlay) -> Self {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        crate::ui::set_uniform_margins(&page, 12);
        page.set_valign(gtk::Align::Start);

        let versions = adw::PreferencesGroup::builder().title("Versions").build();
        let rows = [
            ("Application", env!("CARGO_PKG_VERSION").to_string()),
            ("GTK", state.system.gtk_version.clone()),
            ("libadwaita", state.system.adwaita_version.clone()),
            ("GStreamer", state.system.gstreamer_version.clone()),
        ];
        for (k, v) in rows {
            versions.add(&value_row(k, &v));
        }
        page.append(&versions);

        let decode = adw::PreferencesGroup::builder()
            .title("Video decoding")
            .build();
        for probe in &state.system.decoders {
            let row = value_row(
                &probe.element,
                if probe.available {
                    "available"
                } else {
                    "not installed"
                },
            );
            if !probe.available {
                row.add_css_class("dimmed");
            }
            decode.add(&row);
        }
        decode.add(&value_row(
            "pipewiresink",
            if state.system.pipewire_sink_available {
                "available"
            } else {
                "missing"
            },
        ));
        page.append(&decode);

        let net_group = adw::PreferencesGroup::builder()
            .title("Network interfaces")
            .build();
        if state.system.interfaces.is_empty() {
            net_group.add(&value_row("interfaces", "none detected"));
        }
        for iface in &state.system.interfaces {
            net_group.add(&value_row(&iface.name, &iface.addr));
        }
        page.append(&net_group);

        let metrics_list = gtk::ListBox::new();
        metrics_list.set_selection_mode(gtk::SelectionMode::None);
        metrics_list.add_css_class("boxed-list");
        let metrics_group = adw::PreferencesGroup::builder()
            .title("Live metrics")
            .build();
        metrics_group.add(&metrics_list);
        page.append(&metrics_group);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let save_btn = gtk::Button::with_label("Save diagnostic report");
        save_btn.add_css_class("suggested-action");
        let toast_for_save = toast_overlay.clone();
        let state_for_save = state.clone();
        save_btn.connect_clicked(move |_| match save_report(&state_for_save) {
            Ok(path) => {
                toast_for_save.add_toast(adw::Toast::new(&format!(
                    "Report saved: {}",
                    path.display()
                )));
                tracing::info!(path = %path.display(), "diagnostic report saved");
            }
            Err(e) => {
                toast_for_save.add_toast(adw::Toast::new(&format!("Failed to save report: {e}")));
                tracing::error!(error = %e, "failed to save diagnostic report");
            }
        });
        actions.append(&save_btn);
        page.append(&actions);

        let root = gtk::ScrolledWindow::new();
        root.set_child(Some(&page));
        root.set_vexpand(true);

        Self { root, metrics_list }
    }

    pub fn update_metrics(&self, metrics: &BTreeMap<String, MetricValue>) {
        while let Some(child) = self.metrics_list.first_child() {
            self.metrics_list.remove(&child);
        }
        if metrics.is_empty() {
            self.metrics_list
                .append(&value_row("metrics", "none recorded yet"));
        }
        for (k, v) in metrics {
            let rendered = match v {
                MetricValue::Count(n) => n.to_string(),
                MetricValue::Gauge(x) => format!("{x:.2}"),
                MetricValue::Text(t) => t.clone(),
            };
            self.metrics_list.append(&value_row(k, &rendered));
        }
    }
}

fn value_row(key: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(key).build();
    let lbl = gtk::Label::new(Some(value));
    lbl.add_css_class("monospace");
    row.add_suffix(&lbl);
    row
}

fn save_report(state: &Arc<AppState>) -> anyhow::Result<std::path::PathBuf> {
    let path = report_path()?;
    let mut out = String::new();
    out.push_str("Wireless Display diagnostics\n============================\n\n");
    out.push_str(&format!("application: {}\n", state.system.app_version));
    out.push_str(&format!(
        "gtk: {}  adwaita: {}  gstreamer: {}\n\n",
        state.system.gtk_version, state.system.adwaita_version, state.system.gstreamer_version
    ));
    out.push_str("decoders:\n");
    for d in &state.system.decoders {
        out.push_str(&format!(
            "  {:<14} {}\n",
            d.element,
            if d.available { "yes" } else { "no" }
        ));
    }
    out.push_str("\ninterfaces:\n");
    for i in &state.system.interfaces {
        out.push_str(&format!("  {}: {}\n", i.name, i.addr));
    }
    out.push_str("\nmetrics:\n");
    for (k, v) in state.metrics.snapshot() {
        out.push_str(&format!("  {k}: {v:?}\n"));
    }
    std::fs::write(&path, out)?;
    Ok(path)
}
