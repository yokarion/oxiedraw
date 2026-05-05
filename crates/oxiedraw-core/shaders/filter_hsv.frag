#version 450

// Hue / Saturation / Value adjustment. Reads one premultiplied BGRA
// source (binding 0) and writes the adjusted premultiplied result.
// Operates in the renderer's linear-premultiplied space: the source
// sampler decodes sRGB to linear on read, the framebuffer re-encodes
// on write. Selection mask clipping happens in a separate mask-mix pass.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

layout(push_constant) uniform Push {
    // x = hue rotation in radians, y = saturation multiplier,
    // z = brightness (1.0 = identity), w = unused.
    vec4 params;
} push;

vec3 rgb_to_hsv(vec3 c) {
    vec4 k = vec4(0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0);
    vec4 p = mix(vec4(c.bg, k.wz), vec4(c.gb, k.xy), step(c.b, c.g));
    vec4 q = mix(vec4(p.xyw, c.r), vec4(c.r, p.yzx), step(p.x, c.r));
    float d = q.x - min(q.w, q.y);
    float e = 1.0e-10;
    return vec3(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x);
}

vec3 hsv_to_rgb(vec3 c) {
    vec4 k = vec4(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    vec3 p = abs(fract(c.xxx + k.xyz) * 6.0 - k.www);
    return c.z * mix(k.xxx, clamp(p - k.xxx, 0.0, 1.0), c.y);
}

void main() {
    vec4 src = texture(u_src, v_uv);
    float a = src.a;
    // Unpremultiply so the adjustment operates on straight color.
    vec3 straight = (a > 0.0001) ? src.rgb / a : vec3(0.0);

    vec3 hsv = rgb_to_hsv(clamp(straight, 0.0, 1.0));
    hsv.x = fract(hsv.x + push.params.x / 6.28318530718);
    hsv.y = clamp(hsv.y * push.params.y, 0.0, 1.0);
    vec3 adjusted = hsv_to_rgb(hsv);

    // Brightness: darken (< 1) by scaling, brighten (> 1) by an additive lift
    // so even pixels already at full value keep getting brighter toward white.
    float brightness = push.params.z;
    if (brightness >= 1.0) {
        adjusted += vec3(brightness - 1.0);
    } else {
        adjusted *= brightness;
    }
    adjusted = clamp(adjusted, 0.0, 1.0);

    out_color = vec4(adjusted * a, a);
}
