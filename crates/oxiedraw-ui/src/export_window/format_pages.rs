//! Per-format `adw::PreferencesGroup` builders.
//!
//! Each page returns a `(group, FormatWidgets)` pair. `FormatWidgets`
//! is a tiny dispatcher that fans out widget callbacks to the caller.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::export::settings::{ChromaSubsampling, ExportSettings, PngBitDepth};
use relm4::gtk;

use super::save_settings;

pub(super) struct FormatWidgets {
    on_changed: Rc<RefCell<Vec<Rc<dyn Fn()>>>>,
}

impl FormatWidgets {
    pub(super) fn new() -> Self {
        Self {
            on_changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(super) fn connect_changed(&self, cb: Rc<dyn Fn()>) {
        self.on_changed.borrow_mut().push(cb);
    }
}

pub(super) fn build_png_page(
    settings: &Rc<RefCell<ExportSettings>>,
) -> (adw::PreferencesGroup, FormatWidgets) {
    let group = adw::PreferencesGroup::new();
    let fw = FormatWidgets::new();

    let trans_row = adw::SwitchRow::new();
    trans_row.set_title("Transparency");
    trans_row.set_subtitle("Preserve the alpha channel.");
    trans_row.set_active(settings.borrow().png.transparency);
    group.add(&trans_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        trans_row.connect_active_notify(move |r| {
            s.borrow_mut().png.transparency = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let depth_row = adw::ActionRow::new();
    depth_row.set_title("Bit Depth");
    depth_row.set_subtitle("8-bit is smaller; 16-bit preserves color accuracy.");
    let depth_items = gtk::StringList::new(&["8-bit", "16-bit"]);
    let depth_drop = gtk::DropDown::new(Some(depth_items), gtk::Expression::NONE);
    depth_drop.set_valign(gtk::Align::Center);
    depth_drop.set_selected(match settings.borrow().png.bit_depth {
        PngBitDepth::Eight => 0,
        PngBitDepth::Sixteen => 1,
    });
    depth_row.add_suffix(&depth_drop);
    depth_row.set_activatable_widget(Some(&depth_drop));
    group.add(&depth_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        depth_drop.connect_selected_notify(move |d| {
            s.borrow_mut().png.bit_depth = if d.selected() == 0 {
                PngBitDepth::Eight
            } else {
                PngBitDepth::Sixteen
            };
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let interlace_row = adw::SwitchRow::new();
    interlace_row.set_title("Interlaced");
    interlace_row.set_subtitle("Show progressive scan while loading.");
    interlace_row.set_active(settings.borrow().png.interlaced);
    group.add(&interlace_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        interlace_row.connect_active_notify(move |r| {
            s.borrow_mut().png.interlaced = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let comp_row = adw::ActionRow::new();
    comp_row.set_title("Compression");
    comp_row.set_subtitle("Higher levels render slower but save bytes.");
    let comp_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    comp_box.set_valign(gtk::Align::Center);
    let comp_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 9.0, 1.0);
    comp_slider.set_width_request(120);
    comp_slider.set_draw_value(false);
    comp_slider.set_value(settings.borrow().png.compression as f64);
    let comp_val = gtk::Label::new(Some(&settings.borrow().png.compression.to_string()));
    comp_val.set_width_chars(2);
    comp_val.set_xalign(1.0);
    comp_box.append(&comp_slider);
    comp_box.append(&comp_val);
    comp_row.add_suffix(&comp_box);
    group.add(&comp_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        comp_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            comp_val.set_label(&v.to_string());
            s.borrow_mut().png.compression = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    (group, fw)
}

pub(super) fn build_jpeg_page(
    settings: &Rc<RefCell<ExportSettings>>,
) -> (adw::PreferencesGroup, FormatWidgets) {
    let group = adw::PreferencesGroup::new();
    let fw = FormatWidgets::new();

    let qual_row = adw::ActionRow::new();
    qual_row.set_title("Quality");
    qual_row.set_subtitle("Higher values keep more detail at larger file size.");
    let qual_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    qual_box.set_valign(gtk::Align::Center);
    let qual_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 100.0, 1.0);
    qual_slider.set_width_request(120);
    qual_slider.set_draw_value(false);
    qual_slider.set_value(settings.borrow().jpeg.quality as f64);
    let qual_val = gtk::Label::new(Some(&settings.borrow().jpeg.quality.to_string()));
    qual_val.set_width_chars(3);
    qual_val.set_xalign(1.0);
    qual_box.append(&qual_slider);
    qual_box.append(&qual_val);
    qual_row.add_suffix(&qual_box);
    group.add(&qual_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        qual_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            qual_val.set_label(&v.to_string());
            s.borrow_mut().jpeg.quality = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let blur_row = adw::ActionRow::new();
    blur_row.set_title("Blur");
    blur_row.set_subtitle("Gaussian blur applied before encoding; reduces artifacts.");
    let blur_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    blur_box.set_valign(gtk::Align::Center);
    let blur_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 10.0, 0.1);
    blur_slider.set_width_request(120);
    blur_slider.set_draw_value(false);
    blur_slider.set_value(settings.borrow().jpeg.blur as f64);
    let blur_val = gtk::Label::new(Some(&format!("{:.1}", settings.borrow().jpeg.blur)));
    blur_val.set_width_chars(4);
    blur_val.set_xalign(1.0);
    blur_box.append(&blur_slider);
    blur_box.append(&blur_val);
    blur_row.add_suffix(&blur_box);
    group.add(&blur_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        blur_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation)]
            let v = sl.value() as f32;
            blur_val.set_label(&format!("{v:.1}"));
            s.borrow_mut().jpeg.blur = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let prog_row = adw::SwitchRow::new();
    prog_row.set_title("Progressive");
    prog_row.set_subtitle("Display a low-resolution version while the full image loads.");
    prog_row.set_active(settings.borrow().jpeg.progressive);
    group.add(&prog_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        prog_row.connect_active_notify(move |r| {
            s.borrow_mut().jpeg.progressive = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let cs_row = adw::ActionRow::new();
    cs_row.set_title("Chroma Subsampling");
    cs_row.set_subtitle("4:4:4 preserves full colour; 4:2:0 saves space.");
    let cs_items = gtk::StringList::new(&["4:4:4", "4:2:2", "4:2:0", "4:1:1"]);
    let cs_drop = gtk::DropDown::new(Some(cs_items), gtk::Expression::NONE);
    cs_drop.set_valign(gtk::Align::Center);
    cs_drop.set_selected(match settings.borrow().jpeg.chroma_subsampling {
        ChromaSubsampling::Cs444 => 0,
        ChromaSubsampling::Cs422 => 1,
        ChromaSubsampling::Cs420 => 2,
        ChromaSubsampling::Cs411 => 3,
    });
    cs_row.add_suffix(&cs_drop);
    cs_row.set_activatable_widget(Some(&cs_drop));
    group.add(&cs_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        cs_drop.connect_selected_notify(move |d| {
            s.borrow_mut().jpeg.chroma_subsampling = match d.selected() {
                0 => ChromaSubsampling::Cs444,
                1 => ChromaSubsampling::Cs422,
                3 => ChromaSubsampling::Cs411,
                _ => ChromaSubsampling::Cs420,
            };
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    (group, fw)
}

pub(super) fn build_webp_page(
    settings: &Rc<RefCell<ExportSettings>>,
) -> (adw::PreferencesGroup, FormatWidgets) {
    let group = adw::PreferencesGroup::new();
    let fw = FormatWidgets::new();

    let lossless_row = adw::SwitchRow::new();
    lossless_row.set_title("Lossless");
    lossless_row.set_subtitle("Perfectly reconstruct the original pixels.");
    lossless_row.set_active(settings.borrow().webp.lossless);
    group.add(&lossless_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        lossless_row.connect_active_notify(move |r| {
            s.borrow_mut().webp.lossless = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let qual_row = adw::ActionRow::new();
    qual_row.set_title("Quality");
    qual_row.set_subtitle("Ignored when Lossless is on.");
    let qual_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    qual_box.set_valign(gtk::Align::Center);
    let qual_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    qual_slider.set_width_request(120);
    qual_slider.set_draw_value(false);
    qual_slider.set_value(settings.borrow().webp.quality as f64);
    let qual_val = gtk::Label::new(Some(&settings.borrow().webp.quality.to_string()));
    qual_val.set_width_chars(3);
    qual_val.set_xalign(1.0);
    qual_box.append(&qual_slider);
    qual_box.append(&qual_val);
    qual_row.add_suffix(&qual_box);
    group.add(&qual_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        qual_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            qual_val.set_label(&v.to_string());
            s.borrow_mut().webp.quality = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let trans_row = adw::SwitchRow::new();
    trans_row.set_title("Transparency");
    trans_row.set_subtitle("Include the alpha channel.");
    trans_row.set_active(settings.borrow().webp.transparency);
    group.add(&trans_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        trans_row.connect_active_notify(move |r| {
            s.borrow_mut().webp.transparency = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let effort_row = adw::ActionRow::new();
    effort_row.set_title("Encoder Effort");
    effort_row.set_subtitle("Higher effort means slower encoding and smaller files.");
    let eff_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    eff_box.set_valign(gtk::Align::Center);
    let eff_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 6.0, 1.0);
    eff_slider.set_width_request(120);
    eff_slider.set_draw_value(false);
    eff_slider.set_value(settings.borrow().webp.effort as f64);
    let eff_val = gtk::Label::new(Some(&settings.borrow().webp.effort.to_string()));
    eff_val.set_width_chars(2);
    eff_val.set_xalign(1.0);
    eff_box.append(&eff_slider);
    eff_box.append(&eff_val);
    effort_row.add_suffix(&eff_box);
    group.add(&effort_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        eff_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            eff_val.set_label(&v.to_string());
            s.borrow_mut().webp.effort = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    (group, fw)
}

pub(super) fn build_avif_page(
    settings: &Rc<RefCell<ExportSettings>>,
) -> (adw::PreferencesGroup, FormatWidgets) {
    let group = adw::PreferencesGroup::new();
    let fw = FormatWidgets::new();

    let lossless_row = adw::SwitchRow::new();
    lossless_row.set_title("Lossless");
    lossless_row.set_subtitle("Perfectly reconstruct the original pixels (slow).");
    lossless_row.set_active(settings.borrow().avif.lossless);
    group.add(&lossless_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        lossless_row.connect_active_notify(move |r| {
            s.borrow_mut().avif.lossless = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let qual_row = adw::ActionRow::new();
    qual_row.set_title("Quality");
    qual_row.set_subtitle("Ignored when Lossless is on.");
    let qual_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    qual_box.set_valign(gtk::Align::Center);
    let qual_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    qual_slider.set_width_request(120);
    qual_slider.set_draw_value(false);
    qual_slider.set_value(settings.borrow().avif.quality as f64);
    let qual_val = gtk::Label::new(Some(&settings.borrow().avif.quality.to_string()));
    qual_val.set_width_chars(3);
    qual_val.set_xalign(1.0);
    qual_box.append(&qual_slider);
    qual_box.append(&qual_val);
    qual_row.add_suffix(&qual_box);
    group.add(&qual_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        qual_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            qual_val.set_label(&v.to_string());
            s.borrow_mut().avif.quality = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let trans_row = adw::SwitchRow::new();
    trans_row.set_title("Transparency");
    trans_row.set_subtitle("Include the alpha channel.");
    trans_row.set_active(settings.borrow().avif.transparency);
    group.add(&trans_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        trans_row.connect_active_notify(move |r| {
            s.borrow_mut().avif.transparency = r.is_active();
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    let speed_row = adw::ActionRow::new();
    speed_row.set_title("Encoder Speed");
    speed_row.set_subtitle("Lower speed means better compression but slower encoding.");
    let spd_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    spd_box.set_valign(gtk::Align::Center);
    let spd_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 10.0, 1.0);
    spd_slider.set_width_request(120);
    spd_slider.set_draw_value(false);
    spd_slider.set_value(settings.borrow().avif.speed as f64);
    let spd_val = gtk::Label::new(Some(&settings.borrow().avif.speed.to_string()));
    spd_val.set_width_chars(2);
    spd_val.set_xalign(1.0);
    spd_box.append(&spd_slider);
    spd_box.append(&spd_val);
    speed_row.add_suffix(&spd_box);
    group.add(&speed_row);
    {
        let s = Rc::clone(settings);
        let fw_c = fw.on_changed.clone();
        spd_slider.connect_value_changed(move |sl| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = sl.value().round() as u8;
            spd_val.set_label(&v.to_string());
            s.borrow_mut().avif.speed = v;
            save_settings(&s.borrow());
            for cb in fw_c.borrow().iter() {
                cb();
            }
        });
    }

    (group, fw)
}
