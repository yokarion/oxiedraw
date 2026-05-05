#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;

layout(location = 0) out vec4 out_color;

void main() {
    float d = length(v_local);
    // ~1px feather at small radii, scales gently for larger ones.
    float aa = max(0.75, v_radius * 0.05);
    float coverage = 1.0 - smoothstep(v_radius - aa, v_radius, d);
    // Premultiplied: scale both color and alpha by coverage * flow.
    out_color = v_color * coverage * v_flow;
}
