#version 450

// Liquify warp: resample the pre-liquify layer snapshot through the
// displacement field. This is the only place the warped image is produced -
// both the live preview and the commit run this same pass, so what the user
// sees while dragging is what gets baked.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_source; // layer snapshot, BGRA
layout(set = 0, binding = 1) uniform sampler2D u_field;  // RG16F displacement

layout(push_constant) uniform PC {
    vec4 info; // xy = canvas size in pixels
} pc;

void main() {
    vec2 canvas = pc.info.xy;
    vec2 p = v_uv * canvas;
    vec2 displacement = texture(u_field, v_uv).rg;
    // Clamp-to-edge on the source sampler means a displacement reaching past
    // the canvas replicates the border pixel rather than tearing to transparent.
    out_color = texture(u_source, (p + displacement) / canvas);
}
