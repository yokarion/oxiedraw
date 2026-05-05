#version 450

// Same attribute layout as `dab.vert` so the pipeline can share the
// `DabInstance` vertex buffer.
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec2 a_center;
layout(location = 2) in float a_radius;
layout(location = 3) in float a_rotation;    // ignored
layout(location = 4) in float a_aspect;      // ignored
layout(location = 5) in float a_flow;
layout(location = 6) in vec4 a_color;
layout(location = 7) in vec4 a_texture_uv;   // ignored

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;  // unused on pixel; kept so all dab pipelines share a push layout
} push;

layout(location = 0) out vec2 v_local;
layout(location = 1) out float v_radius;
layout(location = 2) out vec4 v_color;
layout(location = 3) out float v_flow;

void main() {
    // Snap centre to the nearest pixel grid point (pixel (i,j) has its
    // centre at (i+0.5, j+0.5)). Rotation and aspect are dropped on the
    // pixel-art family by design.
    vec2 snapped = floor(a_center) + vec2(0.5);
    vec2 local = a_quad * a_radius;
    vec2 pixel = snapped + local;
    vec2 clip = pixel * push.inv_size - vec2(1.0);
    gl_Position = vec4(clip, 0.0, 1.0);

    v_local = local;
    v_radius = a_radius;
    v_color = a_color;
    v_flow = a_flow;
}
