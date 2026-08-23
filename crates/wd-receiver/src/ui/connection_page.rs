use std::collections::BTreeMap;

use gtk::prelude::*;

use crate::diag::MetricValue;
use crate::session;

pub struct ConnectionPage {
    pub root: adw::StatusPage,
    stats: Vec<(&'static str, gtk::Label)>,
}

const STAT_KEYS: &[&str] = &[
    "video.fps",
    "video.bitrate_kbps",
    "latency.e2e_ms",
    "av.drift_ms",
    "net.rtt_ms",
    "audio.state",
];

impl ConnectionPage {
    pub fn build() -> Self {
        let grid = gtk::Grid::new();
        grid.set_row_spacing(8);
        grid.set_column_spacing(24);
        grid.set_halign(gtk::Align::Center);
        grid.set_margin_top(18);

        let mut stats = Vec::new();
        let titles = [
            "FPS",
            "Video bitrate",
            "Latency",
            "A/V drift",
            "RTT",
            "Audio",
        ];
        for (i, key) in STAT_KEYS.iter().enumerate() {
            let name = gtk::Label::builder()
                .label(titles[i])
                .halign(gtk::Align::End)
                .css_classes(["dimmed"])
                .build();
            let value = gtk::Label::builder()
                .label("—")
                .halign(gtk::Align::Start)
                .css_classes(["monospace"])
                .build();
            grid.attach(&name, 0, i as i32, 1, 1);
            grid.attach(&value, 1, i as i32, 1, 1);
            stats.push((*key, value));
        }

        let root = adw::StatusPage::builder()
            .icon_name("phone-symbolic")
            .title("No active session")
            .description("Waiting for stream")
            .child(&grid)
            .build();

        Self { root, stats }
    }

    pub fn update_state(&self, state: session::State) {
        self.root.set_description(Some(match state {
            session::State::Negotiating => "Negotiating stream…",
            session::State::Streaming => "Streaming",
            session::State::Recovering => "Connection interrupted — recovering",
            session::State::Failed => "Connection failed",
            _ => "Not streaming",
        }));
    }

    pub fn update_peer(&self, peer: Option<&str>) {
        let title = match peer {
            Some(name) => format!("Connected to {name}"),
            None => "No active session".to_string(),
        };
        self.root.set_title(&title);
    }

    pub fn update_metrics(&self, metrics: &BTreeMap<String, MetricValue>) {
        for (key, label) in &self.stats {
            match metrics.get(*key) {
                Some(MetricValue::Gauge(v)) => label.set_text(&format!("{v:.1}")),
                Some(MetricValue::Count(v)) => label.set_text(&v.to_string()),
                Some(MetricValue::Text(t)) => label.set_text(t),
                None => label.set_text("—"),
            }
        }
    }
}
