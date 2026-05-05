#version 450

// Reads from a scratch R8 image (the shape being applied) and writes to
// the selection mask R8 image. The combination of mode (Replace / Add /
// Subtract / Intersect) is encoded purely in the bound pipeline's blend
// state, so this shader just outputs the scratch sample to R.
//
// Replace:   src=ONE,  dst=ZERO,             ADD     -> out = src
// Add:       blend op MAX (factors ignored)          -> out = max(dst, src)
// Subtract:  src=ZERO, dst=ONE_MINUS_SRC_COLOR, ADD  -> out = dst * (1 - src)
// Intersect: blend op MIN (factors ignored)          -> out = min(dst, src)

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

void main() {
    float s = texture(u_src, v_uv).r;
    out_color = vec4(s, 0.0, 0.0, 0.0);
}
