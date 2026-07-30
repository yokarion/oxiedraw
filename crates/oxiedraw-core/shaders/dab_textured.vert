#version 450

// Textured family: a procedural tip (round or square, feathered by
// hardness) modulated by a global grain texture sampled in canvas space.
// The quad is positioned with aspect + rotation; the tip mask uses the
// raw quad corner so it stays shape-stable, and the grain is sampled at
// the fragment's canvas pixel position so it is anchored to the canvas
// (continuous across the whole stroke, not stamped per dab).
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec2 a_center;
layout(location = 2) in float a_radius;
layout(location = 3) in float a_rotation;
layout(location = 4) in float a_aspect;
layout(location = 5) in float a_flow;
layout(location = 6) in vec4 a_color;
layout(location = 7) in vec4 a_texture_uv;       // unused by the global-grain path
layout(location = 8) in float a_hardness;        // tip edge falloff
layout(location = 9) in float a_tip;             // 0 = round, 1 = square
layout(location = 10) in float a_texture_scale;  // grain tile size in canvas px
layout(location = 11) in float a_texture_strength;
layout(location = 12) in float a_texturing_mode; // 0 multiply, 1 subtract

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
} push;

layout(location = 0) out vec2 v_tip;             // raw quad corner [-1,+1]
layout(location = 1) out vec2 v_canvas_px;       // canvas-space pixel position
layout(location = 2) out vec4 v_color;
layout(location = 3) out float v_flow;
layout(location = 4) out float v_hardness;
layout(location = 5) out float v_tip_kind;
layout(location = 6) out float v_tex_scale;
layout(location = 7) out float v_tex_strength;
layout(location = 8) out float v_radius;         // canvas pixels, for edge AA
layout(location = 9) out vec2 v_tip_uv;          // stamped-tip sample coords [0,1]
layout(location = 10) out float v_tex_mode;      // 0 multiply, 1 subtract

void main() {
    // Apply aspect (squish on Y) then rotation, then radius scale.
    vec2 squished = vec2(a_quad.x, a_quad.y * a_aspect);
    float c = cos(a_rotation);
    float s = sin(a_rotation);
    vec2 rotated = vec2(c * squished.x - s * squished.y, s * squished.x + c * squished.y);
    vec2 local = rotated * a_radius;
    vec2 pixel = a_center + local;
    vec2 clip = pixel * push.inv_size - vec2(1.0);
    gl_Position = vec4(clip, 0.0, 1.0);

    v_tip = a_quad;
    v_canvas_px = pixel;
    v_color = a_color;
    v_flow = a_flow;
    v_hardness = a_hardness;
    v_tip_kind = a_tip;
    v_tex_scale = a_texture_scale;
    v_tex_strength = a_texture_strength;
    v_radius = a_radius;
    // Map the raw quad corner [-1,+1] to tip-image UV [0,1]. The tip mask
    // is stamped in dab-local space, so it scales/rotates with the dab.
    v_tip_uv = a_quad * 0.5 + 0.5;
    v_tex_mode = a_texturing_mode;
}
