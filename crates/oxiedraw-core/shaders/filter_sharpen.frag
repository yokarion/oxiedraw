#version 450

// Unsharp-mask sharpen. Combines the original layer (binding 0) with a
// box-blurred copy (binding 1): out = original + amount * (original -
// blurred). Operates on premultiplied color; the result is clamped to
// the valid premultiplied range [0, a] so it stays a legal color.
// Selection clipping is a separate mask-mix pass.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_original;
layout(set = 0, binding = 1) uniform sampler2D u_blurred;

layout(push_constant) uniform Push {
    // x = sharpen amount (0 = no change), yzw unused.
    vec4 params;
} push;

void main() {
    vec4 orig = texture(u_original, v_uv);
    vec4 blur = texture(u_blurred, v_uv);
    float amount = push.params.x;

    vec3 sharp = orig.rgb + amount * (orig.rgb - blur.rgb);
    out_color = vec4(clamp(sharp, 0.0, orig.a), orig.a);
}
