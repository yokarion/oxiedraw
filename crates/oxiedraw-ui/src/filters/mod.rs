//! Filter UI: the Filters menu actions and their popups.
//!
//! Filters run on the GPU via the core `Canvas` filter API. Adjustable
//! filters (HSV, blur, sharpen) open a reusable non-blocking popup
//! ([`dialog::build`]) with a live preview that updates as the user drags
//! the sliders; the layer pixels are only written on Apply. Invert has no
//! parameters, so it applies immediately.
//!
//! Every apply records one undo step (a [`HistoryAction::Batch`] labelled
//! with the filter name) covering all affected layers.

mod dialog;
mod blur;
mod hsv;
mod invert;
mod sharpen;

pub(crate) use blur::show_blur;
pub(crate) use hsv::show_hsv;
pub(crate) use invert::show_invert;
pub(crate) use sharpen::show_sharpen;

use std::cell::Cell;
use std::rc::Rc;
use std::cell::RefCell;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::filters::FilterSpec;
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerPatch};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::canvas::RedrawHandle;
use crate::toaster::Toaster;

/// Shared handles the filter actions need. Cloned into each menu action.
#[derive(Clone)]
pub(crate) struct FilterContext {
    pub window: adw::ApplicationWindow,
    pub canvas: Rc<RefCell<Canvas>>,
    pub redraw: RedrawHandle,
    pub history: Rc<RefCell<HistoryStack>>,
    pub toaster: Toaster,
    pub refresh_layers: Rc<dyn Fn()>,
    /// Returns the ids of the layers the user has selected (including the
    /// active one). Mapped to canvas indices when a filter is invoked.
    pub selected_ids: Rc<dyn Fn() -> Vec<String>>,
}

/// (canvas index, layer id) for each layer a filter will touch.
pub(super) fn affected_layers(ctx: &FilterContext) -> Vec<(usize, String)> {
    let c = ctx.canvas.borrow();
    let snapshot = c.layers().snapshot();
    let wanted = (ctx.selected_ids)();
    let mut out: Vec<(usize, String)> = snapshot
        .iter()
        .enumerate()
        .filter(|(_, l)| wanted.contains(&l.id))
        .map(|(i, l)| (i, l.id.clone()))
        .collect();
    // selected_ids always includes the active layer when one exists, but
    // fall back defensively so a filter is never a silent no-op.
    if out.is_empty() {
        let active = c
            .layers()
            .active()
            .and_then(|a| snapshot.get(a).map(|l| (a, l.id.clone())));
        if let Some((idx, id)) = active {
            out.push((idx, id));
        }
    }
    out
}

/// Apply `spec` to the affected layers: capture before-pixels, run the GPU
/// filter, then record one undoable step. Used by both the immediate filters
/// and the popup Apply buttons. Returns false if there was nothing to do.
pub(super) fn commit(ctx: &FilterContext, affected: &[(usize, String)], spec: FilterSpec) -> bool {
    if affected.is_empty() {
        ctx.toaster.info("No layer to filter");
        return false;
    }
    let indices: Vec<usize> = affected.iter().map(|(i, _)| *i).collect();
    let (w, h) = {
        let c = ctx.canvas.borrow();
        let s = c.size();
        (s.width, s.height)
    };

    // The preview never writes to the layer images, so read_layer here still
    // returns the unfiltered originals - exactly what history needs as "before".
    let befores: Vec<(String, Vec<u8>)> = {
        let mut c = ctx.canvas.borrow_mut();
        affected
            .iter()
            .map(|(idx, id)| (id.clone(), c.read_layer(*idx).unwrap_or_default()))
            .collect()
    };

    {
        let mut c = ctx.canvas.borrow_mut();
        if let Err(e) = c.apply_filter(&indices, spec) {
            tracing::error!(error = %e, "filter apply failed");
            return false;
        }
    }

    let mut actions = Vec::new();
    {
        let mut c = ctx.canvas.borrow_mut();
        for ((idx, _), (id, before)) in affected.iter().zip(befores.iter()) {
            let after = c.read_layer(*idx).unwrap_or_default();
            if let Some(patch) = LayerPatch::from_full_diff(before, &after, w, h) {
                actions.push(HistoryAction::Filter {
                    layer_id: id.clone(),
                    patch,
                });
            }
        }
    }
    if !actions.is_empty() {
        ctx.history.borrow_mut().record(HistoryAction::Batch {
            label: spec.display_name().to_string(),
            actions,
        });
    }

    (ctx.refresh_layers)();
    ctx.redraw.request();
    ctx.toaster.info(&format!("Applied {}", spec.display_name()));
    true
}

/// Open an adjustable filter popup: arm the GPU preview, build the dialog,
/// and wire Apply (commit) / Cancel (drop the preview).
pub(super) fn open_adjustable(
    ctx: &FilterContext,
    title: &str,
    initial: FilterSpec,
    populate: impl FnOnce(&gtk::Box, &Rc<Cell<FilterSpec>>, &FilterContext),
) {
    let affected = affected_layers(ctx);
    if affected.is_empty() {
        ctx.toaster.info("No layer to filter");
        return;
    }
    let indices: Vec<usize> = affected.iter().map(|(i, _)| *i).collect();

    let spec = Rc::new(Cell::new(initial));
    ctx.canvas.borrow_mut().begin_filter(&indices, spec.get());
    ctx.redraw.request();

    let on_apply: Rc<dyn Fn()> = {
        let ctx = ctx.clone();
        let spec = Rc::clone(&spec);
        Rc::new(move || {
            commit(&ctx, &affected, spec.get());
        })
    };
    let on_cancel: Rc<dyn Fn()> = {
        let ctx = ctx.clone();
        Rc::new(move || {
            ctx.canvas.borrow_mut().cancel_filter();
            ctx.redraw.request();
        })
    };

    let dialog = dialog::build(&ctx.window, title, on_apply, on_cancel);
    populate(&dialog.content, &spec, ctx);
    dialog.window.present();
}
