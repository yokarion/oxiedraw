use oxiedraw_core::filters::FilterSpec;

use super::{affected_layers, commit, FilterContext};

pub(crate) fn show_invert(ctx: &FilterContext) {
    let affected = affected_layers(ctx);
    commit(ctx, &affected, FilterSpec::Invert);
}
