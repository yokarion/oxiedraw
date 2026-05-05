#version 450

// Affine transform sampler: maps each output framebuffer pixel back to a
// source-texture UV via a precomputed 2x3 inverse matrix in normalised space,
// then samples with hardware bilinear filtering.
//
// The CPU folds the chain of operations (output→canvas→current-rect-local→
// original-rect-local→source-pixel→source-UV) into a single 2x3 matrix per
// apply call so the shader is a single dot-product + texture sample per pixel.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_source;

layout(push_constant) uniform PC {
    // src_uv.x = row0.x * v_uv.x + row0.y * v_uv.y + row0.z
    // (row0.w is padding for std140 vec4 alignment)
    vec4 row0;
    vec4 row1;
} pc;

void main() {
    vec2 src_uv = vec2(
        pc.row0.x * v_uv.x + pc.row0.y * v_uv.y + pc.row0.z,
        pc.row1.x * v_uv.x + pc.row1.y * v_uv.y + pc.row1.z
    );
    // Outside the source UV bounds: stays transparent. CLAMP_TO_EDGE on the
    // sampler would replicate edge pixels (usually transparent for cleared
    // layers), but the explicit check is cheaper than a fringe sample.
    if (any(lessThan(src_uv, vec2(0.0))) || any(greaterThan(src_uv, vec2(1.0)))) {
        out_color = vec4(0.0);
        return;
    }
    out_color = texture(u_source, src_uv);
}
