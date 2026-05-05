#version 450

// Fill-overlay shader. Reads the per-pixel R8 distance mask uploaded by
// `Canvas::begin_fill_overlay` and renders premultiplied `fill_color`
// wherever the pixel is part of the fill region (mask != sentinel) AND
// the pixel's normalised distance is within the current reveal radius.
//
// One push float controls the radius, swept by the animation timer —
// the layer image stays untouched until the final commit.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

// Distance mask: 8-bit normalised. 1.0 (255/255) = sentinel "outside
// the fill region"; 0.0..(254/255) = normalised Euclidean distance
// from the seed for pixels inside the fill.
layout(set = 0, binding = 0) uniform sampler2D u_fill_mask;

layout(push_constant) uniform Push {
    // rgb = premultiplied fill colour (alpha = 1 since bucket fill is opaque).
    vec4 color;
    // 0.0..(254/255). Pixels with distance <= reveal are revealed.
    float reveal;
} push;

// Threshold below 1.0 to detect the sentinel — any pixel whose mask
// byte was set to 255 should be excluded. We treat values >= 254.5/255
// as outside.
const float SENTINEL_THRESHOLD = 254.5 / 255.0;

void main() {
    float d = texture(u_fill_mask, v_uv).r;
    if (d >= SENTINEL_THRESHOLD) {
        discard;
    }
    if (d > push.reveal) {
        discard;
    }
    out_color = push.color;
}
