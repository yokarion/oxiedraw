#version 450

layout(location = 0) in vec2 v_tip;             // raw quad corner [-1,+1]
layout(location = 1) in vec2 v_canvas_px;       // canvas-space pixel position
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;
layout(location = 5) in float v_tip_kind;
layout(location = 6) in float v_tex_scale;
layout(location = 7) in float v_tex_strength;

layout(set = 0, binding = 0) uniform sampler2DArray u_atlas;

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
} push;

layout(location = 0) out vec4 out_color;

float tip_coverage(vec2 q, float hardness, float kind) {
    float dist = (kind > 0.5) ? max(abs(q.x), abs(q.y)) : length(q);
    float aa = 0.04;
    float inner = min(hardness, 1.0 - aa);
    return 1.0 - smoothstep(inner, 1.0, dist);
}

float grain_factor() {
    if (v_tex_scale <= 0.0 || v_tex_strength <= 0.0) {
        return 1.0;
    }
    vec2 uv = v_canvas_px / v_tex_scale;
    // Force the base mip so the canvas-minified grain stays crisp.
    float g = textureLod(u_atlas, vec3(uv, float(push.slice)), 0.0).a;
    return mix(1.0, g, v_tex_strength);
}

void main() {
    float coverage = tip_coverage(v_tip, v_hardness, v_tip_kind) * grain_factor();
    // v_color is premultiplied; scale colour + alpha by coverage * flow.
    out_color = v_color * coverage * v_flow;
}
