#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;

layout(location = 0) out vec4 out_color;

// Coverage of a soft round dab. hardness=1 -> crisp ~1px feathered edge;
// lower values push the fade inward for a soft/airbrush falloff.
float round_coverage(float d, float radius, float hardness) {
    float aa = max(0.75, radius * 0.05);
    float inner = min(radius * hardness, radius - aa);
    return 1.0 - smoothstep(inner, radius, d);
}

void main() {
    float d = length(v_local);
    float coverage = round_coverage(d, v_radius, v_hardness);
    // Premultiplied: scale both color and alpha by coverage * flow.
    out_color = v_color * coverage * v_flow;
}
