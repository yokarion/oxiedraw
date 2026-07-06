//! Layer actions (keyboard shortcuts + context menu backing) and the
//! clipboard copy / paste implementations.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::history::{
    FoldedLayer, HistoryAction, HistoryStack, LayerPatch, capture_layer,
};
use oxiedraw_core::renderer::RendererError;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::canvas::RedrawHandle;
use crate::clipboard::LayerClipboard;
use crate::toaster::Toaster;

use super::{
    GroupData, LayerNode, RowKind, Ui, commit_groups, compute_visible_rows, find_group,
    find_group_position, group_leaf_ids, group_nodes, insert_at_in_group, item_at, mirror_tree,
    new_group_id, record_tree_edit, sync_canvas_order, sync_height, take_node, tree_to_core,
    ungroup_node,
};

pub(super) fn install_layer_actions(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
    toaster: &Toaster,
    history: &Rc<RefCell<HistoryStack>>,
    prepare_delete: &Rc<dyn Fn() -> bool>,
) {
    let Some(gio_app) = gio::Application::default() else {
        tracing::warn!("install_layer_actions: no default application");
        return;
    };
    let Ok(app) = gio_app.downcast::<gtk::Application>() else {
        tracing::warn!("install_layer_actions: default app is not gtk::Application");
        return;
    };

    // --- Duplicate ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("layer-duplicate", None);
        action.connect_activate(move |_, _| {
            let Some(idx) = ui.state.active() else { return };
            let result = canvas.borrow_mut().duplicate_layer(idx);
            match result {
                Ok(new_idx) => {
                    let (new_id, new_name, new_kind, new_blend, new_opacity, new_pixels) = {
                        let mut c = canvas.borrow_mut();
                        let snap = c.layers().snapshot();
                        let id = snap.get(new_idx).map(|l| l.id.clone()).unwrap_or_default();
                        let name = snap.get(new_idx).map(|l| l.name.clone()).unwrap_or_default();
                        let kind = snap.get(new_idx).map(|l| l.kind.clone()).unwrap_or_default();
                        let blend = snap.get(new_idx).map_or(
                            oxiedraw_core::document::BlendMode::Normal,
                            |l| l.blend,
                        );
                        let opacity = snap.get(new_idx).map_or(1.0, |l| l.opacity);
                        let pixels = c.read_layer(new_idx).unwrap_or_default();
                        (id, name, kind, blend, opacity, pixels)
                    };
                    history.borrow_mut().record(HistoryAction::LayerDuplicate {
                        src_idx: idx,
                        new_idx,
                        new_id,
                        new_name,
                        layer_kind: new_kind,
                        blend: new_blend,
                        opacity: new_opacity,
                        pixels: new_pixels,
                    });
                    sync_height(&area, &ui);
                    commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
                    area.queue_draw();
                    redraw.request();
                    refresh_action_sensitivity(&ui);
                }
                Err(RendererError::LayerLimit) => toaster.layer_limit_reached(),
                Err(e) => tracing::error!(error = %e, "layer-duplicate failed"),
            }
        });
        app.add_action(&action);
    }

    // --- Delete ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let history = Rc::clone(history);
        let prepare_delete = Rc::clone(prepare_delete);
        let action = gio::SimpleAction::new("layer-delete", None);
        action.connect_activate(move |_, _| {
            // Cancel an in-progress transform first so its layer index can't go
            // stale. A paste-via-transform cancel removes its own layer, so
            // there's nothing left to delete afterwards.
            if !prepare_delete() {
                sync_height(&area, &ui);
                area.queue_draw();
                redraw.request();
                refresh_action_sensitivity(&ui);
                return;
            }
            let Some(idx) = ui.state.active() else { return };
            // Capture layer state before deletion for history.
            let pre = {
                let mut c = canvas.borrow_mut();
                let snap = c.layers().snapshot();
                snap.get(idx).and_then(|layer| {
                    let id = layer.id.clone();
                    let name = layer.name.clone();
                    let visible = layer.visible;
                    let kind = layer.kind.clone();
                    let blend = layer.blend;
                    let opacity = layer.opacity;
                    c.read_layer(idx)
                        .ok()
                        .map(|pixels| (id, name, visible, kind, blend, opacity, pixels))
                })
            };
            let result = canvas.borrow_mut().remove_layer(idx);
            match result {
                Ok(()) => {
                    if let Some((id, name, visible, kind, blend, opacity, pixels)) = pre {
                        history.borrow_mut().record(HistoryAction::LayerRemove {
                            idx,
                            id,
                            name,
                            visible,
                            layer_kind: kind,
                            blend,
                            opacity,
                            pixels,
                        });
                    }
                    sync_height(&area, &ui);
                    commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
                    area.queue_draw();
                    redraw.request();
                    refresh_action_sensitivity(&ui);
                }
                Err(e) => tracing::error!(error = %e, "layer-delete failed"),
            }
        });
        app.add_action(&action);
    }

    // --- Group ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("layer-group", None);
        action.connect_activate(move |_, _| {
            let ids = ui.selected_layer_ids_in_order();
            if ids.is_empty() {
                return;
            }
            let before = tree_to_core(&ui.tree.borrow());
            group_nodes(&mut ui.tree.borrow_mut(), &ids, "Group");
            ui.multi_selected.borrow_mut().clear();
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            record_tree_edit(&history, before, tree_to_core(&ui.tree.borrow()), "Group layers");
            sync_height(&area, &ui);
            area.queue_draw();
            refresh_action_sensitivity(&ui);
        });
        app.add_action(&action);
    }

    // --- Merge ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("layers-merge", None);
        action.connect_activate(move |_, _| {
            let selected: Vec<String> = ui.selected_layer_ids_in_order();
            if selected.len() < 2 {
                return;
            }
            let snap = canvas.borrow().layers().snapshot();
            let mut sorted: Vec<usize> = selected
                .iter()
                .filter_map(|id| snap.iter().position(|l| &l.id == id))
                .collect();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() < 2 {
                return;
            }
            let removed_ids: Vec<String> =
                sorted[1..].iter().map(|&i| snap[i].id.clone()).collect();

            // Capture pre-merge state for history.
            let (survivor_pre, folded) = {
                let mut c = canvas.borrow_mut();
                let survivor_pre = c.read_layer(sorted[0]).unwrap_or_default();
                let folded: Vec<FoldedLayer> = sorted[1..].iter()
                    .filter_map(|&i| {
                        let layer = snap.get(i)?;
                        let pixels = c.read_layer(i).ok()?;
                        Some(FoldedLayer {
                            idx: i,
                            id: layer.id.clone(),
                            name: layer.name.clone(),
                            visible: layer.visible,
                            blend: layer.blend,
                            opacity: layer.opacity,
                            pixels,
                        })
                    })
                    .collect();
                (survivor_pre, folded)
            };

            let (survivor_blend, survivor_opacity) = snap
                .get(sorted[0])
                .map_or((oxiedraw_core::document::BlendMode::Normal, 1.0), |l| {
                    (l.blend, l.opacity)
                });

            let result = canvas.borrow_mut().merge_layers(&sorted);
            match result {
                Ok(_) => {
                    let survivor_post = canvas.borrow_mut()
                        .read_layer(sorted[0]).unwrap_or_default();
                    history.borrow_mut().record(HistoryAction::LayerMerge {
                        survivor_idx: sorted[0],
                        survivor_pre,
                        survivor_post,
                        survivor_blend,
                        survivor_opacity,
                        folded,
                    });
                    let mut tree = ui.tree.borrow_mut();
                    for rid in &removed_ids {
                        take_node(&mut tree, rid);
                    }
                    drop(tree);
                    ui.multi_selected.borrow_mut().clear();
                    sync_height(&area, &ui);
                    commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
                    area.queue_draw();
                    redraw.request();
                    refresh_action_sensitivity(&ui);
                }
                Err(e) => tracing::error!(error = %e, "layers-merge failed"),
            }
        });
        app.add_action(&action);
    }

    // --- Group: ungroup ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("group-ungroup", None);
        action.connect_activate(move |_, _| {
            let Some(gid) = ui.active_group.borrow().clone() else { return };
            let before = tree_to_core(&ui.tree.borrow());
            ungroup_node(&mut ui.tree.borrow_mut(), &gid);
            *ui.active_group.borrow_mut() = None;
            sync_canvas_order(&ui.tree.borrow().clone(), &mut canvas.borrow_mut());
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            record_tree_edit(&history, before, tree_to_core(&ui.tree.borrow()), "Ungroup");
            sync_height(&area, &ui);
            area.queue_draw();
            refresh_action_sensitivity(&ui);
        });
        app.add_action(&action);
    }

    // --- Group: delete (group + all its leaf layers) ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("group-delete", None);
        action.connect_activate(move |_, _| {
            let Some(gid) = ui.active_group.borrow().clone() else { return };
            let tree_before = tree_to_core(&ui.tree.borrow());
            // Resolve flat indices first, then remove highest-first so earlier
            // removals don't shift indices we still need.
            let leaves = group_leaf_ids(&ui.tree.borrow(), &gid);
            let snap = canvas.borrow().layers().snapshot();
            let mut indices: Vec<usize> = leaves
                .iter()
                .filter_map(|id| snap.iter().position(|l| &l.id == id))
                .collect();
            indices.sort_unstable();
            indices.dedup();
            let mut removals: Vec<HistoryAction> = Vec::with_capacity(indices.len());
            for idx in indices.into_iter().rev() {
                let captured = capture_layer(&mut canvas.borrow_mut(), idx);
                if let Err(e) = canvas.borrow_mut().remove_layer(idx) {
                    tracing::error!(error = %e, "group-delete: remove_layer failed");
                    continue;
                }
                if let Some((id, name, visible, kind, blend, opacity, pixels)) = captured {
                    removals.push(HistoryAction::LayerRemove {
                        idx,
                        id,
                        name,
                        visible,
                        layer_kind: kind,
                        blend,
                        opacity,
                        pixels,
                    });
                }
            }
            take_node(&mut ui.tree.borrow_mut(), &gid);
            *ui.active_group.borrow_mut() = None;
            ui.multi_selected.borrow_mut().clear();
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            // One undoable unit: the leaf removals plus dropping the empty group
            // node from the folder tree. The tree edit rides last so undo runs it
            // first, restoring the folder before the leaves are re-added into it.
            let tree_after = tree_to_core(&ui.tree.borrow());
            let mut actions = removals;
            if tree_before != tree_after {
                actions.push(HistoryAction::LayerTreeEdit {
                    before: tree_before,
                    after: tree_after,
                });
            }
            if !actions.is_empty() {
                history.borrow_mut().record(HistoryAction::Batch {
                    label: "Delete group".to_string(),
                    actions,
                });
            }
            sync_height(&area, &ui);
            area.queue_draw();
            redraw.request();
            refresh_action_sensitivity(&ui);
        });
        app.add_action(&action);
    }

    // --- Group: duplicate (recursive) ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("group-duplicate", None);
        action.connect_activate(move |_, _| {
            let Some(gid) = ui.active_group.borrow().clone() else { return };

            let group_clone = {
                let tree = ui.tree.borrow();
                find_group(&tree, &gid).cloned()
            };
            let Some(group_clone) = group_clone else { return };

            // Duplicate each leaf on the canvas and remember the src -> new id pairing.
            let mut id_map: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut new_ids: Vec<String> = Vec::new();
            let leaves = super::leaf_ids_top_first(&group_clone.children);
            let mut hit_limit = false;
            for src_id in &leaves {
                let snap = canvas.borrow().layers().snapshot();
                let Some(src_idx) = snap.iter().position(|l| &l.id == src_id) else {
                    continue;
                };
                let new_idx = match canvas.borrow_mut().duplicate_layer(src_idx) {
                    Ok(i) => i,
                    Err(RendererError::LayerLimit) => {
                        hit_limit = true;
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "group-duplicate: duplicate_layer failed");
                        continue;
                    }
                };
                let new_id = canvas
                    .borrow()
                    .layers()
                    .snapshot()
                    .get(new_idx)
                    .map(|l| l.id.clone());
                if let Some(nid) = new_id {
                    id_map.insert(src_id.clone(), nid.clone());
                    new_ids.push(nid);
                }
            }

            let mirror_children = mirror_tree(&group_clone.children, &id_map);
            let new_gid = new_group_id();
            let mirror = LayerNode::Group(GroupData {
                id: new_gid.clone(),
                name: format!("{} copy", group_clone.name),
                expanded: group_clone.expanded,
                visible: group_clone.visible,
                children: mirror_children,
                masked_leaves: std::collections::HashSet::new(),
            });

            // Insert above the source group so the copy stacks on top. The dup'd
            // leaves are already on the canvas but not yet in the panel tree, so
            // this snapshot is the folder tree without the copy group.
            let tree_before = tree_to_core(&ui.tree.borrow());
            let pos = find_group_position(&ui.tree.borrow(), &gid);
            match pos {
                Some((None, idx)) => {
                    ui.tree.borrow_mut().insert(idx, mirror);
                }
                Some((Some(parent), idx)) => {
                    insert_at_in_group(&mut ui.tree.borrow_mut(), mirror, &parent, idx);
                }
                None => {
                    ui.tree.borrow_mut().insert(0, mirror);
                }
            }

            sync_canvas_order(&ui.tree.borrow().clone(), &mut canvas.borrow_mut());
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());

            // Record the created layers as one undoable unit. Capture each new
            // layer at its final index (post-reorder) and order ascending so a
            // redo re-inserts them into the correct positions.
            {
                let mut adds: Vec<(usize, HistoryAction)> = Vec::with_capacity(new_ids.len());
                let mut c = canvas.borrow_mut();
                for nid in &new_ids {
                    let Some(idx) = c.layers().snapshot().iter().position(|l| &l.id == nid)
                    else {
                        continue;
                    };
                    if let Some((id, name, visible, kind, blend, opacity, pixels)) =
                        capture_layer(&mut c, idx)
                    {
                        adds.push((
                            idx,
                            HistoryAction::LayerAdd {
                                idx,
                                id,
                                name,
                                visible,
                                layer_kind: kind,
                                blend,
                                opacity,
                                pixels,
                            },
                        ));
                    }
                }
                drop(c);
                adds.sort_by_key(|(idx, _)| *idx);
                let mut actions: Vec<HistoryAction> = adds.into_iter().map(|(_, a)| a).collect();
                // Fold in the folder-tree change (the new copy group) so one undo
                // removes both the copied layers and their group. Rides last so
                // undo drops the group node before the layers are removed.
                let tree_after = tree_to_core(&ui.tree.borrow());
                if tree_before != tree_after {
                    actions.push(HistoryAction::LayerTreeEdit {
                        before: tree_before,
                        after: tree_after,
                    });
                }
                if !actions.is_empty() {
                    history.borrow_mut().record(HistoryAction::Batch {
                        label: "Duplicate group".to_string(),
                        actions,
                    });
                }
            }

            ui.state.set_active(None);
            *ui.active_group.borrow_mut() = Some(new_gid);
            sync_height(&area, &ui);
            area.queue_draw();
            redraw.request();
            refresh_action_sensitivity(&ui);
            if hit_limit {
                toaster.layer_limit_reached();
            }
        });
        app.add_action(&action);
    }

    // --- Initial sensitivity ---
    refresh_action_sensitivity(ui);

    // --- Copy ---
    {
        let ui = ui.clone();
        let canvas = Rc::clone(canvas);
        let layer_clipboard = Rc::clone(layer_clipboard);
        let action = gio::SimpleAction::new("copy", None);
        action.connect_activate(move |_, _| {
            layer_copy(&ui, &canvas, &layer_clipboard);
        });
        app.add_action(&action);
    }

    // --- Cut ---
    // Copy the active layer (or the selection within it) to the clipboard,
    // then remove the copied pixels from the source layer.
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let layer_clipboard = Rc::clone(layer_clipboard);
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("cut", None);
        action.connect_activate(move |_, _| {
            let Some(idx) = ui.state.active() else { return };
            let had_selection = canvas.borrow().selection_active();
            layer_copy(&ui, &canvas, &layer_clipboard);
            // Capture before-state for history.
            let (layer_id, before_pixels) = {
                let mut c = canvas.borrow_mut();
                let id = c.layers().snapshot().get(idx)
                    .map(|l| l.id.clone()).unwrap_or_default();
                let px = c.read_layer(idx).unwrap_or_default();
                (id, px)
            };
            let result = if had_selection {
                canvas.borrow_mut().clear_selection_from_layer(idx)
            } else {
                canvas.borrow_mut().clear_layer_at(idx, [0.0, 0.0, 0.0, 0.0])
            };
            if let Err(e) = result {
                tracing::error!(error = %e, "cut failed");
                return;
            }
            // Record history for the pixel clear.
            let after_pixels = canvas.borrow_mut().read_layer(idx).unwrap_or_default();
            let cs = canvas.borrow().size();
            if let Some(patch) = LayerPatch::from_full_diff(
                &before_pixels, &after_pixels, cs.width, cs.height,
            ) {
                history.borrow_mut().record(HistoryAction::Clear { layer_id, patch });
            }
            sync_height(&area, &ui);
            area.queue_draw();
            redraw.request();
        });
        app.add_action(&action);
    }

    // --- Paste ---
    {
        let ui = ui.clone();
        let area = area.clone();
        let canvas = Rc::clone(canvas);
        let redraw = redraw.clone();
        let layer_clipboard = Rc::clone(layer_clipboard);
        let toaster = toaster.clone();
        let history = Rc::clone(history);
        let action = gio::SimpleAction::new("paste", None);
        action.connect_activate(move |_, _| {
            layer_paste(&area, &ui, &canvas, &redraw, &layer_clipboard, &toaster, &history);
        });
        app.add_action(&action);
    }
}

/// Copy the active layer (or the masked subset, if a selection is
/// active) to the internal clipboard and write a memory texture to the
/// system clipboard so other apps can receive it.
pub(super) fn layer_copy(
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
) {
    let Some(idx) = ui.state.active() else { return };
    let mut c = canvas.borrow_mut();
    let size = c.size();
    let (pixels, w, h) = if c.selection_active() {
        match c.read_selection_pixels(idx) {
            Ok(Some((px, w, h))) => (px, w, h),
            Ok(None) | Err(_) => {
                if let Ok(px) = c.read_layer(idx) {
                    (px, size.width, size.height)
                } else {
                    return;
                }
            }
        }
    } else {
        match c.read_layer(idx) {
            Ok(px) => (px, size.width, size.height),
            Err(_) => return,
        }
    };
    drop(c);

    let name = ui
        .state
        .snapshot()
        .get(idx)
        .map_or_else(|| "Layer".to_string(), |l| l.name.clone());

    *layer_clipboard.borrow_mut() = Some(LayerClipboard {
        name,
        pixels: pixels.clone(),
        canvas_width: w,
        canvas_height: h,
    });

    // Also push to the system clipboard as a texture so other apps can paste it.
    if let Some(display) = gdk::Display::default() {
        let stride = (w * 4) as usize;
        let bytes = glib::Bytes::from(&pixels);
        #[allow(clippy::cast_possible_wrap)]
        let texture = gdk::MemoryTexture::new(
            w as i32,
            h as i32,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            stride,
        );
        display.clipboard().set_texture(&texture);
    }
}

/// Paste a layer from either the internal clipboard or the system clipboard.
///
/// Internal path (fast, synchronous):
/// - Triggered when the internal LayerClipboard holds pixels that match the
///   current canvas dimensions exactly.
/// - Pixels are copied directly into a new layer via add_layer_with_pixels.
/// - Shows a "Layer pasted!" toast on success.
///
/// External path (slow, asynchronous):
/// - Triggered when there is no matching internal clipboard entry, meaning
///   the image comes from another application or a size-mismatched copy.
/// - Step 1: read_texture_async asks GDK to fetch the system clipboard image.
///   This returns a gdk::Texture on the main thread.
/// - Step 2: a background thread calls texture.save_to_png_bytes() to encode
///   the texture into PNG, then decode_png_bytes() to get raw BGRA pixels.
///   Large images (e.g. 8K) can take several seconds here.
/// - Step 3: idle_add_local polls the mpsc channel each frame until the result
///   arrives, then adds it as a new layer (centred on the canvas) on the main
///   thread.
/// - Toast behavior: a 500 ms one-shot timer runs concurrently with step 2. If
///   the background thread finishes before 500 ms, the done flag is set and the
///   timer callback skips the toast. If 500 ms elapses first, a persistent
///   high-priority toast with a spinner appears. The idle poller sets the done
///   flag and dismisses the toast when the result arrives.
/// - Shows "External image pasted!" on success or an error description on failure.
pub(super) fn layer_paste(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
    toaster: &Toaster,
    history: &Rc<RefCell<HistoryStack>>,
) {
    let canvas_size = canvas.borrow().size();

    // Internal clipboard - exact canvas match means we can restore all data directly.
    if let Some(internal) = layer_clipboard.borrow().as_ref()
        && internal.canvas_width == canvas_size.width
        && internal.canvas_height == canvas_size.height
    {
        let name = format!("{} copy", internal.name);
        let pixels = internal.pixels.clone();
        let result = canvas.borrow_mut().add_layer_with_pixels(name, &pixels);
        match result {
            Ok(new_idx) => {
                if let Some((id, name, visible, kind, blend, opacity, px)) =
                    capture_layer(&mut canvas.borrow_mut(), new_idx)
                {
                    history.borrow_mut().record(HistoryAction::LayerAdd {
                        idx: new_idx,
                        id,
                        name,
                        visible,
                        layer_kind: kind,
                        blend,
                        opacity,
                        pixels: px,
                    });
                }
                sync_height(area, ui);
                commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
                area.queue_draw();
                redraw.request();
                toaster.info("Layer pasted!");
            }
            Err(RendererError::LayerLimit) => toaster.layer_limit_reached(),
            Err(e) => toaster.error(&format!("Failed to paste layer: {e}")),
        }
        return;
    }

    // System clipboard - async texture read.
    let Some(display) = gdk::Display::default() else {
        tracing::warn!("paste: no display");
        return;
    };
    let toaster = toaster.clone();
    let area = area.clone();
    let ui = ui.clone();
    let canvas = Rc::clone(canvas);
    let redraw = redraw.clone();
    let history = Rc::clone(history);

    display
        .clipboard()
        .read_texture_async(gio::Cancellable::NONE, move |result| {
            let texture = match result {
                Ok(Some(t)) => t,
                Ok(None) => {
                    tracing::info!("paste: system clipboard has no image");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "paste: read_texture_async failed");
                    return;
                }
            };

            // 500 ms grace period: only show a "Pasting..." toast if it takes long.
            let done = Rc::new(Cell::new(false));
            let pending_toast: Rc<RefCell<Option<adw::Toast>>> = Rc::new(RefCell::new(None));
            let pending_css: Rc<RefCell<Option<gtk::CssProvider>>> = Rc::new(RefCell::new(None));
            {
                let done_c = Rc::clone(&done);
                let toaster_c = toaster.clone();
                let pending_c = Rc::clone(&pending_toast);
                let css_c = Rc::clone(&pending_css);
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    if done_c.get() {
                        return;
                    }
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let spinner = gtk::Spinner::new();
                    spinner.start();
                    let label = gtk::Label::new(Some("Pasting image..."));
                    row.append(&spinner);
                    row.append(&label);
                    if let Some(t) = toaster_c.pending("") {
                        t.set_custom_title(Some(&row));
                        t.set_priority(adw::ToastPriority::High);
                        *pending_c.borrow_mut() = Some(t);
                    }
                    if let Some(display) = gdk::Display::default() {
                        let provider = gtk::CssProvider::new();
                        provider.load_from_string("toast button.close { display: none; }");
                        gtk::style_context_add_provider_for_display(
                            &display,
                            &provider,
                            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                        );
                        *css_c.borrow_mut() = Some(provider);
                    }
                });
            }

            // Decode the PNG in a background thread; send back raw pixels + dimensions.
            let (tx, rx) = std::sync::mpsc::channel::<Result<(Vec<u8>, u32, u32), String>>();
            std::thread::spawn(move || {
                let png_bytes = texture.save_to_png_bytes();
                match oxiedraw_core::export::decode_png_bytes(&png_bytes) {
                    Some((pixels, src_w, src_h)) => {
                        let _ = tx.send(Ok((pixels, src_w, src_h)));
                    }
                    None => {
                        let _ = tx.send(Err("Could not decode clipboard image".into()));
                    }
                }
            });

            // Idle poller - fires on the main thread when the result is ready.
            glib::idle_add_local(move || {
                let result = match rx.try_recv() {
                    Ok(r) => r,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Err("Worker thread disconnected".into())
                    }
                };

                done.set(true);
                if let Some(t) = pending_toast.borrow_mut().take() {
                    t.dismiss();
                }
                if let Some(provider) = pending_css.borrow_mut().take()
                    && let Some(display) = gdk::Display::default() {
                        gtk::style_context_remove_provider_for_display(&display, &provider);
                    }

                match result {
                    Ok((pixels, src_w, src_h)) => {
                        paste_as_new_layer(
                            &area, &ui, &canvas, &redraw, &toaster, &history, &pixels, src_w,
                            src_h,
                        );
                    }
                    Err(e) => toaster.error(&format!("Paste failed: {e}")),
                }

                glib::ControlFlow::Break
            });
        });
}

// Add the decoded clipboard image as a brand-new layer, centred on the canvas
// at its original size and clipped to the canvas bounds.
fn paste_as_new_layer(
    area: &gtk::DrawingArea,
    ui: &Ui,
    canvas: &Rc<RefCell<Canvas>>,
    redraw: &RedrawHandle,
    toaster: &Toaster,
    history: &Rc<RefCell<HistoryStack>>,
    src: &[u8],
    src_w: u32,
    src_h: u32,
) {
    let size = canvas.borrow().size();
    let pixels = composite_centered(src, src_w, src_h, size.width, size.height);
    let next_n = canvas.borrow().layers().len() + 1;
    let name = format!("Layer {next_n}");
    let result = canvas.borrow_mut().add_layer_with_pixels(name, &pixels);
    match result {
        Ok(new_idx) => {
            if let Some((id, name, visible, kind, blend, opacity, px)) =
                capture_layer(&mut canvas.borrow_mut(), new_idx)
            {
                history.borrow_mut().record(HistoryAction::LayerAdd {
                    idx: new_idx,
                    id,
                    name,
                    visible,
                    layer_kind: kind,
                    blend,
                    opacity,
                    pixels: px,
                });
            }
            sync_height(area, ui);
            commit_groups(&ui.tree.borrow(), &mut canvas.borrow_mut());
            area.queue_draw();
            redraw.request();
            toaster.info("External image pasted!");
        }
        Err(RendererError::LayerLimit) => toaster.layer_limit_reached(),
        Err(e) => toaster.error(&format!("Failed to paste layer: {e}")),
    }
}

// Blit `src` (premultiplied BGRA8, `src_w` x `src_h`) centred into a fresh
// `cw` x `ch` transparent buffer, clipping any part that falls off-canvas.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn composite_centered(src: &[u8], src_w: u32, src_h: u32, cw: u32, ch: u32) -> Vec<u8> {
    let mut buf = vec![0u8; cw as usize * ch as usize * 4];
    let off_x = (cw as i32 - src_w as i32) / 2;
    let off_y = (ch as i32 - src_h as i32) / 2;
    for sy in 0..src_h as i32 {
        let dy = off_y + sy;
        if dy < 0 || dy >= ch as i32 {
            continue;
        }
        let x0 = (-off_x).max(0);
        let x1 = (cw as i32 - off_x).min(src_w as i32);
        if x1 <= x0 {
            continue;
        }
        let s = (sy as usize * src_w as usize + x0 as usize) * 4;
        let e = (sy as usize * src_w as usize + x1 as usize) * 4;
        let d = (dy as usize * cw as usize + (off_x + x0) as usize) * 4;
        buf[d..d + (e - s)].copy_from_slice(&src[s..e]);
    }
    buf
}

// Call after any selection or layer-set change so toolbar buttons follow along.
pub(super) fn refresh_action_sensitivity(ui: &Ui) {
    let Some(gio_app) = gio::Application::default() else { return };
    let Ok(app) = gio_app.downcast::<gtk::Application>() else { return };
    let has_active = ui.state.active().is_some();
    let has_active_group = ui.active_group.borrow().is_some();
    let selected_count = ui.selected_layer_ids_in_order().len();
    for name in &["layer-duplicate", "layer-delete"] {
        if let Some(a) = app.lookup_action(name)
            && let Ok(sa) = a.downcast::<gio::SimpleAction>() {
                sa.set_enabled(has_active);
            }
    }
    if let Some(a) = app.lookup_action("layers-merge")
        && let Ok(sa) = a.downcast::<gio::SimpleAction>() {
            sa.set_enabled(selected_count >= 2);
        }
    if let Some(a) = app.lookup_action("layer-group")
        && let Ok(sa) = a.downcast::<gio::SimpleAction>() {
            sa.set_enabled(selected_count >= 1);
        }
    for name in &["group-ungroup", "group-delete", "group-duplicate"] {
        if let Some(a) = app.lookup_action(name)
            && let Ok(sa) = a.downcast::<gio::SimpleAction>() {
                sa.set_enabled(has_active_group);
            }
    }
}

// ---------------------------------------------------------------------------
// Right-click context menu
// ---------------------------------------------------------------------------

pub(super) fn install_context_menu(
    area: &gtk::DrawingArea,
    ui: &Ui,
    layer_clipboard: &Rc<RefCell<Option<LayerClipboard>>>,
) {
    let layer_menu = gio::Menu::new();
    layer_menu.append(Some("Rename Layer"), Some("app.rename"));
    layer_menu.append(Some("Duplicate Layer"), Some("app.layer-duplicate"));
    layer_menu.append(Some("Copy Layer"), Some("app.copy"));
    layer_menu.append(Some("Delete Layer"), Some("app.layer-delete"));

    // Same as a regular layer, plus the entry that re-opens the effect editor.
    let adjustment_menu = gio::Menu::new();
    adjustment_menu.append(Some("Edit Adjustment"), Some("app.layer-add-adjustment"));
    adjustment_menu.append(Some("Rename Layer"), Some("app.rename"));
    adjustment_menu.append(Some("Duplicate Layer"), Some("app.layer-duplicate"));
    adjustment_menu.append(Some("Copy Layer"), Some("app.copy"));
    adjustment_menu.append(Some("Delete Layer"), Some("app.layer-delete"));

    let group_menu = gio::Menu::new();
    group_menu.append(Some("Rename Group"), Some("app.rename"));
    group_menu.append(Some("Duplicate Group"), Some("app.group-duplicate"));
    group_menu.append(Some("Ungroup"), Some("app.group-ungroup"));
    group_menu.append(Some("Delete Group"), Some("app.group-delete"));

    let popover = Rc::new(gtk::PopoverMenu::from_model(None::<&gio::Menu>));
    popover.set_parent(area);
    popover.set_has_arrow(false);

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    {
        let ui = ui.clone();
        let area_w = area.clone();
        let popover = Rc::clone(&popover);
        let layer_clipboard = Rc::clone(layer_clipboard);
        let layer_menu = layer_menu.clone();
        let adjustment_menu = adjustment_menu.clone();
        let group_menu = group_menu.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);

            let snapshot = ui.state.snapshot();
            let rows = compute_visible_rows(&ui.tree.borrow(), &snapshot);
            // Row hit-testing is in content space; the pointer Y is viewport.
            let Some(row_idx) = item_at(y + ui.vadj.value(), rows.len()) else { return };
            let row = &rows[row_idx];

            // Make the clicked row the sole selection so menu actions target it.
            ui.multi_selected.borrow_mut().clear();
            match &row.kind {
                RowKind::Layer { flat_idx, .. } => {
                    ui.state.select_index(*flat_idx);
                    *ui.active_group.borrow_mut() = None;
                    if super::row_is_adjustment(&ui, row) {
                        popover.set_menu_model(Some(&adjustment_menu));
                    } else {
                        popover.set_menu_model(Some(&layer_menu));
                    }
                }
                RowKind::Group { id, .. } => {
                    *ui.active_group.borrow_mut() = Some(id.clone());
                    ui.state.set_active(None);
                    popover.set_menu_model(Some(&group_menu));
                }
            }
            refresh_action_sensitivity(&ui);
            area_w.queue_draw();

            let has_internal = layer_clipboard.borrow().is_some();
            let has_system_image = gdk::Display::default().is_some_and(|d| {
                d.clipboard().formats().contain_mime_type("image/png")
                    || d.clipboard().formats().contain_mime_type("image/jpeg")
                    || d.clipboard().formats().contain_mime_type("image/webp")
                    || d.clipboard().formats().contain_mime_type("image/bmp")
            });
            if let Some(gio_app) = gio::Application::default()
                && let Ok(app) = gio_app.downcast::<gtk::Application>() {
                    let enabled = has_internal || has_system_image;
                    if let Some(a) = app.lookup_action("paste")
                        && let Ok(sa) = a.downcast::<gio::SimpleAction>() {
                            sa.set_enabled(enabled);
                        }
                }

            #[allow(clippy::cast_possible_truncation)]
            let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            popover.set_pointing_to(Some(&rect));
            popover.popup();
        });
    }
    area.add_controller(click);

    {
        let popover = Rc::clone(&popover);
        area.connect_destroy(move |_| {
            popover.unparent();
        });
    }
}
