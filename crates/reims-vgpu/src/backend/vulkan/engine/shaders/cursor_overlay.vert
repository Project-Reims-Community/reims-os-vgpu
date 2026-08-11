#version 450
layout(push_constant) uniform Push {
    vec4 rect;
    vec2 uv_scale;
} pc;
layout(location = 0) out vec2 v_uv;
void main() {
    vec2 c = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    gl_Position = vec4(mix(pc.rect.xy, pc.rect.zw, c), 0.0, 1.0);
    v_uv = c * pc.uv_scale;
}
