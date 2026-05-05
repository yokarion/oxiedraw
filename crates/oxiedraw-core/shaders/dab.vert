#version 450

// Per-vertex: unit-square corner in [-1, +1].
layout(location = 0) in vec2 a_quad;
// Per-instance.
layout(location = 1) in vec2 a_center;       // canvas pixels
layout(location = 2) in float a_radius;      // canvas pixels
layout(location = 3) in float a_rotation;    // radians (unused by soft-round)
layout(location = 4) in float a_aspect;      // 1.0 = round (unused by soft-round)
layout(location = 5) in float a_flow;        // coverage multiplier, 0..1
layout(location = 6) in vec4 a_color;        // premultiplied linear RGBA
layout(location = 7) in vec4 a_texture_uv;   // (u0,v0,u1,v1) unused until textured family

layout(push_constant) uniform Push {
    vec2 inv_size;  // 2.0 / canvas_size
    uint slice;     // unused on soft-round; kept so all dab pipelines share a push layout
} push;

layout(location = 0) out vec2 v_local;
layout(location = 1) out float v_radius;
layout(location = 2) out vec4 v_color;
layout(location = 3) out float v_flow;

void main() {
    vec2 local = a_quad * a_radius;
    vec2 pixel = a_center + local;
    // Vulkan clip space: x in [-1,+1] left→right, y in [-1,+1] top→bottom.
    // pixel.y == 0 maps to y = -1 (top of canvas), which matches the
    // image-coordinate convention the brush engine uses.
    vec2 clip = pixel * push.inv_size - vec2(1.0);
    gl_Position = vec4(clip, 0.0, 1.0);

    v_local = local;
    v_radius = a_radius;
    v_color = a_color;
    v_flow = a_flow;
}
