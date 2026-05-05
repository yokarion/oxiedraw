#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_stroke;
// Selection mask: 1.0 = pixel selected (stroke writes through),
// 0.0 = pixel masked (stroke suppressed). When no selection is active
// `selection_active` is 0 and the mask sample is ignored.
layout(set = 0, binding = 1) uniform sampler2D u_selection;

layout(push_constant) uniform Push {
    // rgb = linear stroke color, a = stroke opacity multiplier
    vec4 color_opacity;
    // 0.0 = no selection (apply everywhere); 1.0 = multiply coverage by mask.
    float selection_active;
} push;

void main() {
    float coverage = texture(u_stroke, v_uv).r;
    float mask = texture(u_selection, v_uv).r;
    coverage *= mix(1.0, mask, push.selection_active);
    float a = coverage * push.color_opacity.a;
    // Premultiplied output: src.rgb = a * color.
    out_color = vec4(push.color_opacity.rgb * a, a);
}
