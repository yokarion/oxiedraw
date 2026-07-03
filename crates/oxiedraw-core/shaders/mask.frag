#version 450

// Same vertex outputs as the dab pipeline. v_color is unused on the mask pass.
layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;

layout(location = 0) out vec4 out_color;

void main() {
    float d = length(v_local);
    // hardness=1 -> crisp ~1px edge; lower pushes the fade inward (soft).
    float aa = max(0.75, v_radius * 0.05);
    float inner = min(v_radius * v_hardness, v_radius - aa);
    float coverage = 1.0 - smoothstep(inner, v_radius, d);
    // R8_UNORM stroke buffer keeps only the R channel. Coverage * flow goes
    // there; combined across overlapping dabs by the MAX blend op so
    // the mask saturates instead of accumulating.
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
