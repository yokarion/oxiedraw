#version 450

// Fill-overlay shader. The fill is already committed to the layer by the
// time this runs - the reveal animation works by *hiding* the part that
// has not been reached yet, so the pixels on screen sweep from the old
// content to the new one without the layer ever being in a half state.
//
// Which is why there are two pipelines. A fill that went in underneath
// the layer is hidden by taking its share back out (DST_OUT); one that
// replaced the region is hidden by painting the seed colour back over it
// (OVER). One push float controls the radius, swept by the timer.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

// R = distance, 8-bit normalised: 1.0 (255/255) = sentinel "outside
// the fill region"; 0.0..(254/255) = normalised Euclidean distance
// from the seed for pixels inside the fill.
// G = the share of the pixel the fill took, which is exactly the share
// that has to be undone to show what was there before.
layout(set = 0, binding = 0) uniform sampler2D u_fill_mask;

layout(push_constant) uniform Push {
    // Premultiplied colour to paint back over the un-revealed pixels.
    // Ignored by the DST_OUT pipeline, which only uses the alpha.
    vec4 color;
    // 0.0..(254/255). Pixels with distance <= reveal are revealed, and
    // a revealed pixel is left alone - the layer already holds it.
    float reveal;
} push;

// Threshold below 1.0 to detect the sentinel - any pixel whose mask
// byte was set to 255 should be excluded. We treat values >= 254.5/255
// as outside.
const float SENTINEL_THRESHOLD = 254.5 / 255.0;

void main() {
    vec2 m = texture(u_fill_mask, v_uv).rg;
    float d = m.r;
    float share = m.g;
    if (d >= SENTINEL_THRESHOLD) {
        discard;
    }
    if (d <= push.reveal) {
        discard;
    }
    if (share <= 0.0) {
        discard;
    }
    // push.color is premultiplied with alpha 1.0, so scaling the whole
    // vec4 keeps it premultiplied for either blend.
    out_color = push.color * share;
}
