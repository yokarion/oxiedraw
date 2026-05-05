#version 450

// Fullscreen triangle from gl_VertexIndex — no vertex buffer.
// Three verts: (-1,-1), (3,-1), (-1,3). The interior triangle covers
// the entire [-1,+1]^2 framebuffer.
layout(location = 0) out vec2 v_uv;

void main() {
    vec2 p = vec2(
        (gl_VertexIndex == 1) ? 3.0 : -1.0,
        (gl_VertexIndex == 2) ? 3.0 : -1.0
    );
    gl_Position = vec4(p, 0.0, 1.0);
    // UV in [0, 1] across the canvas; origin at top-left to match
    // Vulkan's y-down clip convention and the image-coordinate
    // convention used by the brush engine.
    v_uv = (p + vec2(1.0)) * 0.5;
}
