use adw::prelude::*;
use gtk::{Align, Orientation};

use crate::net::PairedDevice;

pub struct DevicesPage {
    pub root: adw::ToolbarView,
    paired_group: adw::PreferencesGroup,
    /// Rows we added to `paired_group` (removal must skip the group's
    /// internal widgets, so we track them ourselves).
    paired_rows: std::cell::RefCell<Vec<adw::ActionRow>>,
    status_row: adw::ActionRow,
    address_row: adw::ActionRow,
    fingerprint_row: adw::ActionRow,
}

impl DevicesPage {
    pub fn build(
        status_line: &str,
        address_line: &str,
        fingerprint_short: &str,
        on_start_pairing: impl Fn() + 'static,
    ) -> Self {
        let this_receiver = adw::PreferencesGroup::builder()
            .title("This computer")
            .description("Phones connect to this machine; nothing to discover here.")
            .build();

        let status_row = adw::ActionRow::builder()
            .title("Status")
            .subtitle(status_line)
            .build();
        let status_icon = gtk::Image::from_icon_name("network-wireless-signal-excellent-symbolic");
        status_icon.add_css_class("success");
        status_row.add_prefix(&status_icon);

        let address_row = adw::ActionRow::builder()
            .title("Address")
            .subtitle(address_line)
            .build();
        address_row.add_suffix(&gtk::Label::new(Some("QUIC")));

        let fingerprint_row = adw::ActionRow::builder()
            .title("Fingerprint")
            .subtitle(fingerprint_short)
            .tooltip_text("First 12 hex digits of the SHA-256 certificate fingerprint")
            .build();
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy full fingerprint")
            .valign(Align::Center)
            .build();
        fingerprint_row.add_suffix(&copy);

        this_receiver.add(&status_row);
        this_receiver.add(&address_row);
        this_receiver.add(&fingerprint_row);

        let pair_button = gtk::Button::builder()
            .label("Start pairing…")
            .css_classes(["suggested-action"])
            .halign(Align::End)
            .build();
        pair_button.connect_clicked(move |_| on_start_pairing());
        let pair_row = adw::ActionRow::builder()
            .title("Pair a phone")
            .subtitle("Shows a 6-digit code valid for two minutes")
            .build();
        pair_row.add_suffix(&pair_button);
        this_receiver.add(&pair_row);

        let paired_group = adw::PreferencesGroup::builder()
            .title("Paired phones")
            .description("Phones you have paired with this computer.")
            .build();

        let content = gtk::Box::new(Orientation::Vertical, 18);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.set_valign(Align::Start);
        content.append(&this_receiver);
        content.append(&paired_group);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&content));
        scroll.set_vexpand(true);

        let root = adw::ToolbarView::new();
        root.set_content(Some(&scroll));

        Self {
            root,
            paired_group,
            status_row,
            address_row,
            fingerprint_row,
        }
    }

    pub fn set_status(&self, line: &str) {
        self.status_row.set_subtitle(line);
    }

    pub fn set_address(&self, line: &str) {
        self.address_row.set_subtitle(line);
    }

    pub fn set_fingerprint(&self, short: &str) {
        self.fingerprint_row.set_subtitle(short);
    }

    pub fn update_devices(&self, devices: &[PairedDevice]) {
        while let Some(child) = self.paired_group.first_child() {
            self.paired_group.remove(&child);
        }
        if devices.is_empty() {
            let hint = adw::ActionRow::builder()
                .title("No phones paired yet")
                .subtitle("Use “Start pairing…” above, then pair from the phone app.")
                .build();
            hint.add_css_class("dimmed");
            self.paired_group.add(&hint);
            return;
        }
        for device in devices {
            let row = adw::ActionRow::builder()
                .title(device.name.clone())
                .subtitle(format!(
                    "paired device {}",
                    &device.device_id[..8.min(device.device_id.len())]
                ))
                .build();
            let badge = gtk::Label::new(Some("trusted"));
            badge.add_css_class("success");
            row.add_suffix(&badge);
            self.paired_group.add(&row);
        }
    }
}
