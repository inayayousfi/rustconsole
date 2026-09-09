#version 450

layout(set = 0, binding = 0) uniform sampler2D glyph_atlas;
layout(location = 0) in vec2 texture_coordinate;
layout(location = 1) in vec4 text_color;
layout(location = 0) out vec4 output_color;

void main() {
    float coverage = texture_coordinate.x < 0.0
        ? 1.0
        : texture(glyph_atlas, texture_coordinate).r;
    output_color = vec4(text_color.rgb, text_color.a * coverage);
}
