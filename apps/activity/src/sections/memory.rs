use super::top::By;
use crate::charts::{self};
use crate::graph::{self, Scale, Series, Tone};
use crate::widgets::{self, Page};
use crate::{fmt, live, parts};
use gtk::prelude::*;

/// "The last minute", "The last 10 minutes": what the graphs reach back to.
pub fn last_title() -> String {
    match crate::prefs::get().history_secs {
        s if s <= 60 => "The last minute".into(),
        s => format!("The last {} minutes", s / 60),
    }
}

pub fn build(page: &Page) {
    let g = page.cards("");

    let head = parts::head("Memory", "");
    head.root.append(&parts::range());
    g.add(&head.root);

    // ----- Readings -----
    let used = charts::tile("In use");
    let free = charts::tile("Free for apps");
    let swap = charts::tile("Swap and zram");
    let psi = charts::tile("Waiting for memory");
    free.root.set_tooltip_text(Some("What apps could have now: free memory and the file cache the kernel would give back."));
    g.add(&parts::readings(&[&used, &free, &swap, &psi]));

    // ----- Where it goes -----
    const COLOURS: &[&str] = &["amber", "blue", "pink", "violet", "teal"];
    const NAMES: [&str; 5] = ["Apps", "Compressed swap", "Shared and tmpfs", "Kernel", "Cache"];
    let comp = parts::card("", "Where it goes");
    let freed = widgets::label("", "page-card-good");
    freed.add_css_class("tnum");
    comp.head.append(&freed);
    let (bar, split) = super::dashboard::split_bar(COLOURS);
    bar.set_content_height(16);
    comp.root.append(&bar);
    let (keys, key_labels) = super::dashboard::key_row(&NAMES.iter().zip(COLOURS).map(|(n, c)| (*n, *c)).collect::<Vec<_>>());
    comp.root.append(&parts::wrap_keys(&keys));
    comp.root.set_tooltip_text(Some(
        "Apps: memory programs asked for (heaps, stacks).\nShared and tmpfs: shared memory and files in RAM disks like /tmp.\n\
         Compressed swap: what zram keeps in RAM for the memory it has packed away.\n\
         Kernel: memory the kernel keeps for itself.\nCache: copies of files, given back as soon as apps need it.",
    ));
    g.add(&comp.root);

    live::on_tick(&head.root, move |s| {
        let m = &s.mem;
        if m.total == 0 {
            return;
        }
        let t = m.total as f64;
        let mut line = format!("{} of RAM", fmt::bytes(t));
        line.push_str(&match (m.swap_total, m.zram_used > 0) {
            (0, _) => " · no swap".into(),
            (n, true) => format!(" · {} of swap, compressed in RAM by zram", fmt::bytes(n as f64)),
            (n, false) => format!(" · {} of swap", fmt::bytes(n as f64)),
        });
        head.line.set_text(&line);
        let pct = m.used as f64 / t * 100.0;
        used.value.set_text(&fmt::bytes(m.used as f64));
        used.sub.set_text(&format!("of {} · {}", fmt::bytes(t), fmt::pct(pct)));
        widgets::set_warn(&used.value, pct >= crate::prefs::get().warn_mem);
        free.value.set_text(&fmt::bytes(m.available as f64));
        free.sub.set_text("cache counts as free");
        if m.swap_total > 0 {
            swap.value.set_text(&fmt::pct(m.swap_used as f64 / m.swap_total as f64 * 100.0));
            swap.sub.set_text(&if m.zram_used > 0 {
                format!("packed {:.1} to 1", m.zram_data as f64 / m.zram_used as f64)
            } else {
                format!("{} of {}", fmt::bytes(m.swap_used as f64), fmt::bytes(m.swap_total as f64))
            });
        } else {
            swap.value.set_text("None");
            swap.sub.set_text("when RAM runs out, apps are ended");
        }
        match m.pressure {
            Some(p) => {
                psi.value.set_text(&fmt::pct(p.some10));
                psi.sub.set_text("held up, last 10 s");
                widgets::set_warn(&psi.value, p.some10 >= 10.0);
            }
            None => {
                psi.value.set_text("–");
                psi.sub.set_text("not reported by this kernel");
            }
        }
        let cache = (m.cached + m.buffers) as f64;
        let values = vec![m.anon as f64, m.zram_used as f64, m.shared as f64, m.kernel as f64, cache];
        if split.borrow().0 != values {
            *split.borrow_mut() = (values.clone(), t);
            bar.queue_draw();
        }
        for (i, (l, (name, v))) in key_labels.iter().zip(NAMES.iter().zip(values)).enumerate() {
            l.set_markup(&format!("{name} <b>{}</b>", fmt::bytes(v)));
            // Without zram there's nothing compressed to show.
            if let Some(child) = l.parent().and_then(|item| item.parent()) {
                child.set_visible(i != 1 || v > 0.0);
            }
        }
        freed.set_text(&format!("{} could be freed for apps", fmt::bytes(m.available as f64)));
    });

    // ----- Who's using it, and the last few minutes -----
    let users = super::cpu::using_it_most("memory", By::Mem, 6);
    let last = parts::card("", &last_title());
    let gr = graph::graph(
        vec![
            Series::new("mem", "In use", Tone::Swatch("amber")),
            Series { dashed: true, ..Series::new("swap", "Swap and zram", Tone::Swatch("teal")) },
        ],
        Scale::Percent,
        fmt::pct,
        140,
    );
    last.root.append(&gr.root);
    let title = last.head.first_child().and_downcast::<gtk::Label>();
    live::on_tick(&last.root, move |_| {
        if let Some(t) = &title {
            t.set_text(&last_title());
        }
    });
    g.add(&parts::columns(&[users.upcast_ref(), last.root.upcast_ref()], 2));
}
