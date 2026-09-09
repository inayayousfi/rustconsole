#version 450

layout(push_constant) uniform OverlayPush {
    vec4 rect;
    vec4 uv;
    vec4 color;
} overlay;

layout(location = 0) out vec2 texture_coordinate;
layout(location = 1) out vec4 text_color;

const vec2 positions[6] = vec2[](
    vec2(0.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 0.0),
    vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 1.0)
);

void main() {
    vec2 corner = positions[gl_VertexIndex];
    gl_Position = vec4(mix(overlay.rect.xy, overlay.rect.zw, corner), 0.0, 1.0);
    texture_coordinate = mix(overlay.uv.xy, overlay.uv.zw, corner);
    text_color = overlay.color;
}
