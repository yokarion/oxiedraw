//! Extract Palette window: pull the colours out of the drawing and save them
//! as a preset.
//!
//! Each source is analysed once into a [`PaletteSource`], so dragging a slider
//! only re-runs the selection over the cached peaks and repaints the preview.
//! Nothing is written until Add.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::palettes::{
    BackgroundMode, ExtractOptions, ExtractOrder, ExtractedColor, PaletteSource, PaletteState,
};
use relm4::gtk;

use super::shared::{self, PalettePreview};
use crate::toaster::Toaster;
use crate::widgets::swatch_grid::{GridContent, SwatchGrid};

const WINDOW_WIDTH: i32 = 560;
const WINDOW_HEIGHT: i32 = 720;
const SLIDER_WIDTH: i32 = 180;

/// Where the colours are sampled from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Canvas,
    Layer,
    Selection,
}

impl EnumMeta for Source {
    const ALL: &'static [Self] = &[Self::Canvas, Self::Layer, Self::Selection];

    fn label(self) -> &'static str {
        match self {
            Self::Canvas => "Whole canvas",
            Self::Layer => "Active layer",
            Self::Selection => "Selection",
        }
    }
}

impl Source {
    const fn empty_hint(self) -> &'static str {
        match self {
            Self::Canvas => "Nothing to sample - the canvas is empty at these settings.",
            Self::Layer => "Nothing to sample - the active layer is empty at these settings.",
            Self::Selection => "Nothing to sample - make a selection first.",
        }
    }
}

#[derive(Clone)]
struct Extractor {
    canvas: Rc<RefCell<Canvas>>,
    options: Rc<RefCell<ExtractOptions>>,
    source: Rc<Cell<Source>>,
    analysis: Rc<RefCell<Option<(Source, PaletteSource)>>>,
    result: Rc<RefCell<Vec<ExtractedColor>>>,
    grid: Rc<SwatchGrid>,
    colors_group: adw::PreferencesGroup,
    count: gtk::Label,
    preview: Rc<PalettePreview>,
}

impl Extractor {
    /// Re-select and repaint. Re-reads the canvas only when the source changed;
    /// a slider move works off the peaks already in hand.
    fn refresh(&self) {
        let wanted = self.source.get();
        if self.analysis.borrow().as_ref().is_none_or(|(s, _)| *s != wanted) {
            let read = analyze(&self.canvas, wanted).map(|source| (wanted, source));
            *self.analysis.borrow_mut() = read;
        }

        let extracted = self
            .analysis
            .borrow()
            .as_ref()
            .map_or_else(Vec::new, |(_, source)| source.select(&self.options.borrow()));

        self.count.set_text(&match extracted.len() {
            1 => "1 color".to_string(),
            n => format!("{n} colors"),
        });
        self.colors_group
            .set_description(extracted.is_empty().then_some(wanted.empty_hint()));
        self.preview.set_weighted(
            &extracted
                .iter()
                .map(|c| (c.color, c.weight))
                .collect::<Vec<_>>(),
        );
        self.grid.set_content(GridContent {
            colors: extracted.iter().map(|c| c.color).collect(),
            ..GridContent::default()
        });
        *self.result.borrow_mut() = extracted;
    }
}

pub(crate) fn show(
    parent: &adw::ApplicationWindow,
    palettes: &PaletteState,
    canvas: &Rc<RefCell<Canvas>>,
    toaster: &Toaster,
) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(WINDOW_WIDTH)
        .default_height(WINDOW_HEIGHT)
        .title("Extract Palette")
        .build();
    // Esc cancels, as it would in a dialog. Bubble phase, so an open dropdown
    // takes its own Esc first.
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    window.add_controller(keys);

    let grid = Rc::new(SwatchGrid::new());
    let count = gtk::Label::builder()
        .css_classes(["dim-label", "caption"])
        .valign(gtk::Align::Center)
        .build();
    let extractor = Extractor {
        canvas: Rc::clone(canvas),
        options: Rc::new(RefCell::new(ExtractOptions::default())),
        source: Rc::new(Cell::new(Source::Canvas)),
        analysis: Rc::new(RefCell::new(None)),
        result: Rc::new(RefCell::new(Vec::new())),
        colors_group: shared::colors_group(&grid, Some(count.upcast_ref())),
        grid,
        count,
        preview: Rc::new(PalettePreview::new()),
    };
    let refresh: Rc<dyn Fn()> = {
        let extractor = extractor.clone();
        Rc::new(move || extractor.refresh())
    };

    let name_row = adw::EntryRow::builder()
        .title("Name")
        .text("Canvas Palette")
        .build();

    let body = shared::body();
    body.append(&build_settings(&name_row, &extractor, &refresh));
    body.append(&extractor.colors_group);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&build_header(
        &window,
        palettes,
        &extractor.result,
        &name_row,
        toaster,
    ));
    content.append(&shared::scroller(&body));
    content.append(&extractor.preview.widget());
    window.set_content(Some(&content));

    extractor.refresh();
    window.present();
}

fn build_header(
    window: &adw::Window,
    palettes: &PaletteState,
    result: &Rc<RefCell<Vec<ExtractedColor>>>,
    name_row: &adw::EntryRow,
    toaster: &Toaster,
) -> adw::HeaderBar {
    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();

    // Weak throughout: a button in the window holding the window strongly is a
    // loop GTK never breaks, and this one would pin a copy of the canvas
    // pixels and the document's renderer with it.
    let cancel = gtk::Button::with_label("Cancel");
    {
        let window = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    header.pack_start(&cancel);

    let add = gtk::Button::builder()
        .label("Add")
        .css_classes(["suggested-action"])
        .build();
    {
        let window = window.downgrade();
        let palettes = palettes.clone();
        let result = Rc::clone(result);
        let name_row = name_row.clone();
        let toaster = toaster.clone();
        add.connect_clicked(move |_| {
            let colors: Vec<_> = result.borrow().iter().map(|c| c.color).collect();
            if colors.is_empty() {
                toaster.error("Nothing to add - those settings found no colors");
                return;
            }
            let count = colors.len();
            let name = palettes.edit(|store| {
                let name = store.add_palette(name_row.text().trim(), colors);
                store.active_preset.clone_from(&name);
                name
            });
            toaster.info(&format!("Added palette \"{name}\" with {count} colors"));
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    header.pack_end(&add);

    header
}

fn build_settings(
    name_row: &adw::EntryRow,
    extractor: &Extractor,
    refresh: &Rc<dyn Fn()>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.add(name_row);

    let source_row = adw::ComboRow::builder()
        .title("Source")
        .model(&gtk::StringList::new(&Source::labels()))
        .build();
    {
        let source = Rc::clone(&extractor.source);
        let refresh = Rc::clone(refresh);
        source_row.connect_selected_notify(move |row| {
            source.set(Source::from_index(row.selected()));
            refresh();
        });
    }
    group.add(&source_row);

    let defaults = *extractor.options.borrow();

    let background_row = adw::ComboRow::builder()
        .title("Background")
        .subtitle("What to do with the backdrop behind the drawing")
        .model(&gtk::StringList::new(&BackgroundMode::labels()))
        .selected(defaults.background.to_index())
        .build();
    {
        let options = Rc::clone(&extractor.options);
        let refresh = Rc::clone(refresh);
        background_row.connect_selected_notify(move |row| {
            options.borrow_mut().background = BackgroundMode::from_index(row.selected());
            refresh();
        });
    }
    group.add(&background_row);

    group.add(&slider_row(
        "Detail",
        (0.0, 100.0),
        f64::from(defaults.detail) * 100.0,
        |value| format!("{value:.0}%"),
        extractor,
        refresh,
        |options, value| options.detail = (value / 100.0) as f32,
    ));
    group.add(&order_row(defaults.order, extractor, refresh));

    group
}

fn slider_row(
    title: &str,
    range: (f64, f64),
    initial: f64,
    format: impl Fn(f64) -> String + 'static,
    extractor: &Extractor,
    refresh: &Rc<dyn Fn()>,
    apply: impl Fn(&mut ExtractOptions, f64) + 'static,
) -> adw::ActionRow {
    let options = Rc::clone(&extractor.options);
    let refresh = Rc::clone(refresh);
    let scale = crate::widgets::slider::build(range, 1.0, initial, SLIDER_WIDTH, format, move |value| {
        apply(&mut options.borrow_mut(), value);
        refresh();
    });
    scale.set_valign(gtk::Align::Center);
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(&scale);
    row
}

fn order_row(initial: ExtractOrder, extractor: &Extractor, refresh: &Rc<dyn Fn()>) -> adw::ActionRow {
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .build();
    let mut group: Option<gtk::ToggleButton> = None;
    for variant in ExtractOrder::ALL {
        let button = gtk::ToggleButton::builder().label(variant.label()).build();
        // Grouped before the active one is set: joining a group clears it.
        if let Some(first) = &group {
            button.set_group(Some(first));
        } else {
            group = Some(button.clone());
        }
        button.set_active(*variant == initial);
        {
            let options = Rc::clone(&extractor.options);
            let refresh = Rc::clone(refresh);
            let variant = *variant;
            button.connect_toggled(move |button| {
                if !button.is_active() {
                    return;
                }
                options.borrow_mut().order = variant;
                refresh();
            });
        }
        buttons.append(&button);
    }
    let row = adw::ActionRow::builder().title("Order").build();
    row.add_suffix(&buttons);
    row
}

/// Read the chosen source and analyse it. `None` when there is nothing to
/// sample - no active layer, or no selection.
fn analyze(canvas: &Rc<RefCell<Canvas>>, source: Source) -> Option<PaletteSource> {
    let mut canvas = canvas.borrow_mut();
    let (bgra, mask) = match source {
        Source::Canvas => (canvas.read_pixels().ok()?, None),
        Source::Layer => {
            let index = canvas.layers().active()?;
            (canvas.read_layer(index).ok()?, None)
        }
        Source::Selection => {
            if !canvas.selection_active() {
                return None;
            }
            // Not `.ok()`: a dropped mask would quietly extract from the whole
            // canvas, and turn background detection on, while the UI says
            // Selection. Better to report nothing to sample.
            (canvas.read_pixels().ok()?, Some(canvas.read_selection_mask().ok()?))
        }
    };
    let width = canvas.size().width;
    drop(canvas);
    Some(PaletteSource::analyze(&bgra, width, mask.as_deref()))
}
