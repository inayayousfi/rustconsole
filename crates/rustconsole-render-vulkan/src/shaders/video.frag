#version 450

layout(set = 0, binding = 0) uniform sampler2D y_plane;
layout(set = 0, binding = 1) uniform sampler2D uv_plane;
layout(location = 0) in vec2 texture_coordinate;
layout(location = 0) out vec4 output_color;

layout(push_constant) uniform ColorParameters {
    uint mode;
    float source_peak_nits;
    float target_peak_nits;
    float target_black_nits;
} color;

const float pq_m1 = 2610.0 / 16384.0;
const float pq_m2 = 2523.0 / 32.0;
const float pq_c1 = 3424.0 / 4096.0;
const float pq_c2 = 2413.0 / 128.0;
const float pq_c3 = 2392.0 / 128.0;

float pq_encode(float nits) {
    float p = pow(max(nits, 0.0) / 10000.0, pq_m1);
    return pow((pq_c1 + pq_c2 * p) / (1.0 + pq_c3 * p), pq_m2);
}

float pq_decode(float encoded) {
    float p = pow(max(encoded, 0.0), 1.0 / pq_m2);
    return 10000.0 * pow(max(p - pq_c1, 0.0) / (pq_c2 - pq_c3 * p), 1.0 / pq_m1);
}

vec3 pq_decode(vec3 encoded) {
    return vec3(pq_decode(encoded.r), pq_decode(encoded.g), pq_decode(encoded.b));
}

float bt709_inverse_oetf(float encoded) {
    encoded = max(encoded, 0.0);
    return encoded < 0.081 ? encoded / 4.5 : pow((encoded + 0.099) / 1.099, 1.0 / 0.45);
}

vec3 limited_bt709_to_rgb(vec3 yuv) {
    float y = (yuv.x * 255.0 - 16.0) / 219.0;
    vec2 uv = (yuv.yz * 255.0 - vec2(128.0)) / 224.0;
    vec3 encoded = vec3(y + 1.5748 * uv.y,
                        y - 0.187324 * uv.x - 0.468124 * uv.y,
                        y + 1.8556 * uv.x);
    return vec3(bt709_inverse_oetf(encoded.r),
                bt709_inverse_oetf(encoded.g),
                bt709_inverse_oetf(encoded.b));
}

vec3 limited_bt2020_pq_to_rgb(vec3 yuv) {
    const float sampled_max = 65535.0 / 64.0;
    float y = (yuv.x * sampled_max - 64.0) / 876.0;
    vec2 uv = (yuv.yz * sampled_max - vec2(512.0)) / 896.0;
    return vec3(y + 1.4746 * uv.y,
                y - 0.164553 * uv.x - 0.571353 * uv.y,
                y + 1.8814 * uv.x);
}

float bt2390_curve(float encoded_luminance) {
    float output_white = pq_encode(color.target_peak_nits);
    float output_black = pq_encode(color.target_black_nits);
    float input_white = max(pq_encode(color.source_peak_nits), output_white + 0.001);
    float input_black = min(pq_encode(0.001), output_black - 0.001);
    float minimum = (output_black - input_black) / (input_white - input_black);
    float maximum = (output_white - input_black) / (input_white - input_black);
    float knee = 1.5 * maximum - 0.5;
    float mapped = (encoded_luminance - input_black) / (input_white - input_black);
    if (mapped >= knee) {
        float t = (mapped - knee) / (1.0 - knee);
        float t2 = t * t;
        float t3 = t2 * t;
        mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
               + (t3 - 2.0 * t2 + t) * (1.0 - knee)
               + (-2.0 * t3 + 3.0 * t2) * maximum;
    }
    mapped += minimum * pow(1.0 - mapped, 4.0);
    return clamp(mapped * (input_white - input_black) + input_black,
                 output_black, output_white);
}

vec3 bt2390_to_srgb(vec3 pq_rgb) {
    vec3 rgb2020_nits = pq_decode(pq_rgb);
    float input_luminance = dot(rgb2020_nits, vec3(0.2627002, 0.6779981, 0.0593017));
    float output_luminance = pq_decode(bt2390_curve(pq_encode(input_luminance)));
    vec3 mapped2020 = rgb2020_nits * (output_luminance / max(input_luminance, 0.000001));
    vec3 mapped709 = mat3(1.660491, -0.124550, -0.018151,
                         -0.587641, 1.132900, -0.100579,
                         -0.072850, -0.008349, 1.118730) * mapped2020;
    return clamp(mapped709 / color.target_peak_nits, 0.0, 1.0);
}

void main() {
    vec3 yuv = vec3(texture(y_plane, texture_coordinate).r,
                    texture(uv_plane, texture_coordinate).rg);
    if (color.mode == 0) {
        output_color = vec4(limited_bt709_to_rgb(yuv), 1.0);
        return;
    }
    vec3 pq_rgb = limited_bt2020_pq_to_rgb(yuv);
    output_color = vec4(color.mode == 1 ? clamp(pq_rgb, 0.0, 1.0)
                                       : bt2390_to_srgb(pq_rgb),
                        1.0);
}
