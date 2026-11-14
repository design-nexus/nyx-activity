//! Settings as a card over the window: the groups listed on the left, the
//! chosen group's options on the right, and a search across all of them.

use crate::sections::settings;
use crate::{prefs, widgets, window};
use gtk::prelude::*;
use nyx_ui::settings_card::{self, About, Spec, Standard};

pub use nyx_ui::settings_card::{escape, fit, focus_search, is_open};

/// Each settings group's icon and one-line summary in the dialog's list.
const GROUPS: &[(&str, &str, &str)] = &[
    ("Monitoring", "activity-cpu-symbolic", "Update rate, history, units"),
    ("Warnings", "dialog-warning-symbolic", "Temperature, memory, disk"),
    ("Alerts", "preferences-system-notifications-symbolic", "Hot CPU, low memory, failures"),
    ("Processes", "view-list-symbolic", "Ending tasks, kernel threads"),
];

const SPEC: Spec = Spec {
    groups: GROUPS,
    search_text: None,
    standard: Some(Standard {
        app: "activity",
        about: About {
            name: "Activity",
            icon: "io.github.design_nexus.nyx.Activity",
            version: env!("CARGO_PKG_VERSION"),
            description: "Processes, live performance graphs, startup items and services.",
            licence: "MIT",
            website: "https://github.com/design-nexus/nyx-activity",
            issues: "https://github.com/design-nexus/nyx-activity/issues",
            credits: &[],
        },
        shortcuts: window::SHORTCUTS,
        reduce_motion: || prefs::get().reduce_motion,
        set_reduce_motion: |on| prefs::update(|p| p.reduce_motion = on),
    }),
};

pub fn open() {
    let (Some(overlay), Some(window)) = (window::overlay(), window::window()) else { return };
    settings_card::open(&overlay, &window, SPEC, || {
        let page = widgets::page("settings");
        settings::build(&page);
        (page.root.upcast(), page.body)
    });
}
