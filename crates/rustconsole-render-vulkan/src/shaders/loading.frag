#version 450

layout(push_constant) uniform LoadingState {
    float elapsed_seconds;
    float aspect_ratio;
    float hdr10_output;
} loading;

layout(location = 0) in vec2 texture_coordinate;
layout(location = 0) out vec4 output_color;

void main() {
    vec2 point = texture_coordinate * 2.0 - 1.0;
    point.x *= loading.aspect_ratio;

    float radius = length(point);
    float angle = atan(point.y, point.x);
    float phase = fract(angle / 6.2831853 - loading.elapsed_seconds * 0.45);
    float ring = 1.0 - smoothstep(0.012, 0.026, abs(radius - 0.16));
    float trail = mix(0.16, 1.0, phase);

    float head_angle = loading.elapsed_seconds * 2.8274334;
    vec2 head_position = vec2(cos(head_angle), sin(head_angle)) * 0.16;
    float head = exp(-900.0 * dot(point - head_position, point - head_position));

    vec3 background = mix(vec3(0.035, 0.037, 0.044), vec3(0.012, 0.013, 0.016), smoothstep(0.0, 0.8, radius));
    vec3 accent = vec3(1.0, 0.19, 0.35);
    vec3 color = background + accent * (ring * trail * 0.8 + head * 0.7);
    if (loading.hdr10_output > 0.5) {
        const float m1 = 2610.0 / 16384.0;
        const float m2 = 2523.0 / 32.0;
        const float c1 = 3424.0 / 4096.0;
        const float c2 = 2413.0 / 128.0;
        const float c3 = 2392.0 / 128.0;
        vec3 p = pow(max(color, vec3(0.0)) * (203.0 / 10000.0), vec3(m1));
        color = pow((c1 + c2 * p) / (1.0 + c3 * p), vec3(m2));
    }
    output_color = vec4(color, 1.0);
}
