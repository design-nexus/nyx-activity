//! Theming: the shared Nyx theme engine, given this app's stylesheet.

pub use nyx_theme::*;

use crate::prefs;

/// Install the stylesheet and the current palette, and follow Omarchy live.
pub fn install() {
    nyx_theme::use_design_2();
    nyx_theme::install(Style {
        window_class: "activity-window",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "\n.settings-option.search-hit, .theme-card.selected, .toast { box-shadow: 0 0 0 1px alpha(@theme_accent, 0.6); }\n\
             window.activity-window scale > trough > slider { box-shadow: 0 0 0 1px alpha(@theme_accent, 0.72); }\n",
        extra_colors: None,
        prefs: || {
            let p = prefs::get();
            ThemePrefs { mode: p.mode, theme: p.theme, glow: p.glow, reduce_motion: p.reduce_motion }
        },
        set_theme: Some(|mode, theme| {
            prefs::update(|p| {
                p.mode = mode;
                p.theme = theme;
            })
        }),
    });
}
