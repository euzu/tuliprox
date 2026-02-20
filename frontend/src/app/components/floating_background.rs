use fastrand::Rng;
use js_sys::Float32Array;
use std::{cell::RefCell, f32::consts::TAU, rc::Rc};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{window, HtmlCanvasElement, WebGlProgram, WebGlRenderingContext, WebGlShader};
use yew::prelude::*;

const PARTICLE_COUNT: i32 = 520;
const PARTICLE_STRIDE_FLOATS: i32 = 6;
const PARTICLE_STRIDE_BYTES: i32 = PARTICLE_STRIDE_FLOATS * 4;

const VERTEX_SHADER: &str = r#"
attribute vec2 a_base;   // x in clip-space, y as random seed
attribute vec4 a_meta;   // size, speed, phase, layer

uniform float u_time;

varying float v_alpha;
varying float v_layer;

void main() {
    float size = a_meta.x;
    float speed = a_meta.y;
    float phase = a_meta.z;
    float layer = a_meta.w;

    float fall = 1.25 - mod(u_time * speed + a_base.y * 2.8 + phase * 0.2, 2.5);
    float wind = sin(u_time * (0.18 + layer * 0.05) + phase) * (0.035 + layer * 0.02);
    float swirl = sin(u_time * (0.9 + layer * 0.17) + phase * 1.7) * (0.012 + layer * 0.006);

    float x = a_base.x + wind + swirl;
    float y = fall;

    gl_Position = vec4(x, y, 0.0, 1.0);
    gl_PointSize = size * (1.0 + layer * 0.38);

    v_alpha = 0.30 + layer * 0.18;
    v_layer = layer;
}
"#;

const FRAGMENT_SHADER: &str = r#"
precision highp float;

varying float v_alpha;
varying float v_layer;

void main() {
    vec2 uv = gl_PointCoord - vec2(0.5);
    float dist = length(uv);
    if (dist > 0.5) {
        discard;
    }

    float core = smoothstep(0.32, 0.0, dist);
    float halo = smoothstep(0.5, 0.2, dist);
    float alpha = (core * 0.75 + halo * 0.25) * v_alpha;
    vec3 color = mix(vec3(0.45, 0.63, 0.88), vec3(0.95, 0.97, 1.0), v_layer * 0.35);

    gl_FragColor = vec4(color, alpha);
}
"#;

fn build_particle_data() -> Vec<f32> {
    let mut rng = Rng::with_seed(24_021_313);
    let mut data = Vec::with_capacity(PARTICLE_COUNT as usize * PARTICLE_STRIDE_FLOATS as usize);

    for _ in 0..PARTICLE_COUNT {
        let layer = match rng.u32(0..100) {
            0..=43 => 0.0,
            44..=79 => 1.0,
            _ => 2.0,
        };

        let (size, speed) = if layer < 0.5 {
            (rng.f32() * 1.7 + 1.3, rng.f32() * 0.18 + 0.18)
        } else if layer < 1.5 {
            (rng.f32() * 2.4 + 2.2, rng.f32() * 0.24 + 0.28)
        } else {
            (rng.f32() * 3.0 + 3.0, rng.f32() * 0.32 + 0.44)
        };

        let base_x = rng.f32() * 2.4 - 1.2;
        let seed = rng.f32() * 3.0;
        let phase = rng.f32() * TAU;

        data.extend_from_slice(&[base_x, seed, size, speed, phase, layer]);
    }

    data
}

fn resize_canvas(canvas: &HtmlCanvasElement, gl: &WebGlRenderingContext) {
    let Some(win) = window() else { return };

    let width = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1.0);
    let height = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(1.0);
    let dpr = win.device_pixel_ratio().max(1.0);

    let target_width = (width * dpr).round() as u32;
    let target_height = (height * dpr).round() as u32;

    if canvas.width() != target_width || canvas.height() != target_height {
        canvas.set_width(target_width);
        canvas.set_height(target_height);
    }

    gl.viewport(0, 0, target_width as i32, target_height as i32);
}

#[function_component]
pub fn FloatingBackground() -> Html {
    let canvas_ref = use_node_ref();

    {
        let canvas_ref = canvas_ref.clone();
        use_effect_with((), move |_| {
            let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let context = canvas
                .get_context("webgl")
                .ok()
                .flatten()
                .or_else(|| canvas.get_context("experimental-webgl").ok().flatten());
            let Some(context) = context else {
                log::error!("WebGL context is not available for floating background");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let Ok(gl) = context.dyn_into::<WebGlRenderingContext>() else {
                log::error!("Failed to cast WebGL context");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let Ok(program) = link_program(&gl, VERTEX_SHADER, FRAGMENT_SHADER) else {
                log::error!("Failed to compile/link floating background shaders");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            gl.use_program(Some(&program));
            gl.enable(WebGlRenderingContext::BLEND);
            gl.blend_func(
                WebGlRenderingContext::SRC_ALPHA,
                WebGlRenderingContext::ONE_MINUS_SRC_ALPHA,
            );
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            resize_canvas(&canvas, &gl);

            let Some(buffer) = gl.create_buffer() else {
                log::error!("Failed to create floating background buffer");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };
            gl.bind_buffer(WebGlRenderingContext::ARRAY_BUFFER, Some(&buffer));

            let particle_data = build_particle_data();
            let particle_array = Float32Array::from(particle_data.as_slice());
            gl.buffer_data_with_array_buffer_view(
                WebGlRenderingContext::ARRAY_BUFFER,
                &particle_array,
                WebGlRenderingContext::STATIC_DRAW,
            );

            let a_base_loc = gl.get_attrib_location(&program, "a_base");
            if a_base_loc < 0 {
                log::error!("Attribute a_base not found");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            gl.enable_vertex_attrib_array(a_base_loc as u32);
            gl.vertex_attrib_pointer_with_i32(
                a_base_loc as u32,
                2,
                WebGlRenderingContext::FLOAT,
                false,
                PARTICLE_STRIDE_BYTES,
                0,
            );

            let a_meta_loc = gl.get_attrib_location(&program, "a_meta");
            if a_meta_loc < 0 {
                log::error!("Attribute a_meta not found");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            gl.enable_vertex_attrib_array(a_meta_loc as u32);
            gl.vertex_attrib_pointer_with_i32(
                a_meta_loc as u32,
                4,
                WebGlRenderingContext::FLOAT,
                false,
                PARTICLE_STRIDE_BYTES,
                8,
            );

            let u_time = gl.get_uniform_location(&program, "u_time");
            if u_time.is_none() {
                log::error!("Uniform u_time not found");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }

            let raf_id = Rc::new(RefCell::new(None::<i32>));
            let frame_start = Rc::new(RefCell::new(None::<f64>));
            let animation = Rc::new(RefCell::new(None::<Closure<dyn FnMut(f64)>>));

            let gl_anim = gl.clone();
            let animation_ref = animation.clone();
            let raf_id_ref = raf_id.clone();
            let frame_start_ref = frame_start.clone();
            let time_loc = u_time.clone();

            *animation.borrow_mut() = Some(Closure::wrap(Box::new(move |timestamp: f64| {
                let start = {
                    let mut start_ref = frame_start_ref.borrow_mut();
                    match *start_ref {
                        Some(start) => start,
                        None => {
                            *start_ref = Some(timestamp);
                            timestamp
                        }
                    }
                };
                let elapsed = ((timestamp - start) / 1000.0) as f32;

                gl_anim.clear(WebGlRenderingContext::COLOR_BUFFER_BIT);
                gl_anim.uniform1f(time_loc.as_ref(), elapsed);
                gl_anim.draw_arrays(WebGlRenderingContext::POINTS, 0, PARTICLE_COUNT);

                if let Some(win) = window() {
                    if let Some(callback) = animation_ref.borrow().as_ref() {
                        if let Ok(id) = win.request_animation_frame(callback.as_ref().unchecked_ref()) {
                            *raf_id_ref.borrow_mut() = Some(id);
                        }
                    }
                }
            }) as Box<dyn FnMut(f64)>));

            let Some(win) = window() else {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let animation_kickoff = animation.clone();
            if let Some(callback) = animation_kickoff.borrow().as_ref() {
                if let Ok(id) = win.request_animation_frame(callback.as_ref().unchecked_ref()) {
                    *raf_id.borrow_mut() = Some(id);
                }
            }

            let resize_canvas_ref = canvas.clone();
            let resize_gl = gl.clone();
            let on_resize = Closure::<dyn FnMut()>::wrap(Box::new(move || {
                resize_canvas(&resize_canvas_ref, &resize_gl);
            }));
            let _ = win.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());

            Box::new(move || {
                if let Some(win) = window() {
                    if let Some(id) = *raf_id.borrow() {
                        let _ = win.cancel_animation_frame(id);
                    }
                    let _ =
                        win.remove_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
                }
                *animation.borrow_mut() = None;
            }) as Box<dyn FnOnce()>
        });
    }

    html! { <canvas class="tp__floating-background" ref={canvas_ref} aria-hidden="true" /> }
}

fn compile_shader(
    gl: &WebGlRenderingContext,
    shader_type: u32,
    source: &str,
) -> Result<WebGlShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .ok_or_else(|| String::from("Failed to create shader"))?;

    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);

    let status = gl
        .get_shader_parameter(&shader, WebGlRenderingContext::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false);

    if status {
        Ok(shader)
    } else {
        Err(gl.get_shader_info_log(&shader).unwrap_or_else(|| String::from("Unknown shader error")))
    }
}

fn link_program(
    gl: &WebGlRenderingContext,
    vert_source: &str,
    frag_source: &str,
) -> Result<WebGlProgram, String> {
    let vertex_shader = compile_shader(gl, WebGlRenderingContext::VERTEX_SHADER, vert_source)?;
    let fragment_shader = compile_shader(gl, WebGlRenderingContext::FRAGMENT_SHADER, frag_source)?;

    let program = gl
        .create_program()
        .ok_or_else(|| String::from("Failed to create program"))?;

    gl.attach_shader(&program, &vertex_shader);
    gl.attach_shader(&program, &fragment_shader);
    gl.link_program(&program);

    let status = gl
        .get_program_parameter(&program, WebGlRenderingContext::LINK_STATUS)
        .as_bool()
        .unwrap_or(false);

    if status {
        Ok(program)
    } else {
        Err(gl.get_program_info_log(&program).unwrap_or_else(|| String::from("Unknown link error")))
    }
}
