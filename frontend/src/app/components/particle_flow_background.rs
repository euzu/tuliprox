use fastrand::Rng;
use js_sys::Float32Array;
use std::{cell::RefCell, f32::consts::TAU, rc::Rc};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{window, HtmlCanvasElement, WebGlProgram, WebGlRenderingContext, WebGlShader};
use yew::prelude::*;

const PARTICLE_COUNT: i32 = 520;
const PARTICLE_STRIDE_FLOATS: i32 = 5;
const PARTICLE_STRIDE_BYTES: i32 = PARTICLE_STRIDE_FLOATS * 4;
const SPAWN_BOUND: f32 = 1.08;
const WORLD_BOUND: f32 = 1.24;
const DENSITY_GRID_W: usize = 24;
const DENSITY_GRID_H: usize = 16;

#[derive(Clone)]
struct ParticleState {
    pos: [f32; 2],
    vel: [f32; 2],
    home: [f32; 2],
    size: f32,
    alpha: f32,
    layer: f32,
    seed: f32,
}

const VERTEX_SHADER: &str = r#"
attribute vec2 a_pos;
attribute vec3 a_meta; // size, alpha, layer

varying float v_alpha;
varying float v_layer;

void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    gl_PointSize = a_meta.x * (1.0 + a_meta.z * 0.38);
    v_alpha = a_meta.y;
    v_layer = a_meta.z;
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

fn normalize(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt().max(0.00001);
    [v[0] / len, v[1] / len]
}

fn fract(v: f32) -> f32 { v - v.floor() }

fn hash1(v: f32) -> f32 { fract((v.sin() * 43_758.547).abs()) }

fn pos_to_cell(pos: [f32; 2]) -> (usize, usize) {
    let ux = ((pos[0] + WORLD_BOUND) / (WORLD_BOUND * 2.0)).clamp(0.0, 0.999_9);
    let uy = ((pos[1] + WORLD_BOUND) / (WORLD_BOUND * 2.0)).clamp(0.0, 0.999_9);
    let cx = (ux * DENSITY_GRID_W as f32) as usize;
    let cy = (uy * DENSITY_GRID_H as f32) as usize;
    (cx, cy)
}

fn respawn_particle(particle: &mut ParticleState, time: f32) {
    let r = hash1(particle.seed * 13.17 + time * 0.31);
    let t = hash1(particle.seed * 7.11 + time * 0.23) * 2.0 - 1.0;
    let inward = 0.05 + particle.layer * 0.03;

    let side = (r * 4.0).floor() as i32;
    match side {
        0 => {
            particle.pos = [-SPAWN_BOUND, t * SPAWN_BOUND];
            particle.vel = [inward, 0.0];
        }
        1 => {
            particle.pos = [SPAWN_BOUND, t * SPAWN_BOUND];
            particle.vel = [-inward, 0.0];
        }
        2 => {
            particle.pos = [t * SPAWN_BOUND, -SPAWN_BOUND];
            particle.vel = [0.0, inward];
        }
        _ => {
            particle.pos = [t * SPAWN_BOUND, SPAWN_BOUND];
            particle.vel = [0.0, -inward];
        }
    }
    particle.home = particle.pos;
}

fn sample_curl_velocity(pos: [f32; 2], time: f32) -> [f32; 2] {
    let x = pos[0];
    let y = pos[1];
    let mut vx = 0.0;
    let mut vy = 0.0;

    let modes = [
        (0.52_f32, 1.15_f32, 0.84_f32, 0.13_f32, 0.2_f32),
        (0.34_f32, 1.92_f32, -1.47_f32, 0.09_f32, 1.4_f32),
        (0.22_f32, -2.41_f32, 2.18_f32, 0.06_f32, 2.3_f32),
    ];

    for (amp, kx, ky, wt, phase) in modes {
        let a = kx * x + ky * y + wt * time + phase;
        let c = a.cos();
        vx += amp * ky * c;
        vy += -amp * kx * c;

        let bx = kx * 0.71;
        let by = ky * -1.19;
        let b = bx * x + by * y + wt * 1.36 * time + phase * 1.71;
        let cb = b.cos();
        vx += amp * 0.55 * by * cb;
        vy += -amp * 0.55 * bx * cb;
    }

    let len = (vx * vx + vy * vy).sqrt();
    if len > 1.25 {
        let s = 1.25 / len;
        vx *= s;
        vy *= s;
    }

    [vx * 0.24, vy * 0.24]
}

fn build_particles() -> Vec<ParticleState> {
    let mut rng = Rng::with_seed(24_021_313);
    let mut particles = Vec::with_capacity(PARTICLE_COUNT as usize);

    for _ in 0..PARTICLE_COUNT {
        let layer = match rng.u32(0..100) {
            0..=43 => 0.0,
            44..=79 => 1.0,
            _ => 2.0,
        };

        let (size, alpha) = if layer < 0.5 {
            (rng.f32() * 1.7 + 1.3, rng.f32() * 0.10 + 0.18)
        } else if layer < 1.5 {
            (rng.f32() * 2.4 + 2.2, rng.f32() * 0.12 + 0.28)
        } else {
            (rng.f32() * 3.0 + 3.0, rng.f32() * 0.14 + 0.40)
        };

        let seed = rng.f32() * TAU;
        let pos = [rng.f32() * (SPAWN_BOUND * 2.0) - SPAWN_BOUND, rng.f32() * (SPAWN_BOUND * 2.0) - SPAWN_BOUND];

        particles.push(ParticleState { pos, vel: [0.0, 0.0], home: pos, size, alpha, layer, seed });
    }

    particles
}

fn update_particles(particles: &mut [ParticleState], dt: f32, time: f32) {
    let wind_dir = normalize([1.0 + 0.2 * (time * 0.04).sin(), 0.18 * (time * 0.07).sin()]);
    let wind_env = 0.38 + 0.62 * (0.5 + 0.5 * (time * 0.08 + (time * 0.023).sin() * 2.0).sin());
    let expected_density = particles.len() as f32 / (DENSITY_GRID_W * DENSITY_GRID_H) as f32;

    let mut density = [0.0_f32; DENSITY_GRID_W * DENSITY_GRID_H];
    for particle in particles.iter() {
        let (cx, cy) = pos_to_cell(particle.pos);
        density[cy * DENSITY_GRID_W + cx] += 1.0;
    }

    for particle in particles {
        let mut target = sample_curl_velocity(particle.pos, time + particle.seed * 0.25);

        let regional =
            ((particle.pos[0] * 1.6 + time * 0.18).sin() * (particle.pos[1] * 1.3 - time * 0.14).cos()) * 0.5 + 0.5;
        let gust = (0.04 + particle.layer * 0.03) * wind_env * regional;
        target[0] += wind_dir[0] * gust;
        target[1] += wind_dir[1] * gust;

        let layer_scale = 0.62 + particle.layer * 0.18;
        target[0] *= layer_scale;
        target[1] *= layer_scale;

        let (cx, cy) = pos_to_cell(particle.pos);
        let sample = |dx: isize, dy: isize| -> f32 {
            let nx = (cx as isize + dx).clamp(0, DENSITY_GRID_W as isize - 1) as usize;
            let ny = (cy as isize + dy).clamp(0, DENSITY_GRID_H as isize - 1) as usize;
            density[ny * DENSITY_GRID_W + nx]
        };

        let grad_x = (sample(1, 0) - sample(-1, 0)) / expected_density.max(0.0001);
        let grad_y = (sample(0, 1) - sample(0, -1)) / expected_density.max(0.0001);
        let local_density = sample(0, 0) / expected_density.max(0.0001);
        let pressure = (local_density - 1.0).max(0.0);
        let density_push = (0.012 + particle.layer * 0.006) * pressure;
        target[0] += -grad_x * density_push;
        target[1] += -grad_y * density_push;

        // Weak spring to each particle's home area keeps global density even over time.
        let home_pull = 0.011 + particle.layer * 0.004;
        target[0] += (particle.home[0] - particle.pos[0]) * home_pull;
        target[1] += (particle.home[1] - particle.pos[1]) * home_pull;

        // Keep particles from drifting too far for too long, preserving visible density.
        let edge = particle.pos[0].abs().max(particle.pos[1].abs());
        if edge > 0.80 {
            let inward_dir = normalize([-particle.pos[0], -particle.pos[1]]);
            let pull = (edge - 0.80) * 0.10;
            target[0] += inward_dir[0] * pull;
            target[1] += inward_dir[1] * pull;
        }

        let relax = 1.0 - (-dt * (1.8 + particle.layer * 0.65)).exp();
        particle.vel[0] += (target[0] - particle.vel[0]) * relax;
        particle.vel[1] += (target[1] - particle.vel[1]) * relax;

        particle.pos[0] += particle.vel[0] * dt;
        particle.pos[1] += particle.vel[1] * dt;

        if particle.pos[0].abs() > WORLD_BOUND || particle.pos[1].abs() > WORLD_BOUND {
            respawn_particle(particle, time);
        }
    }
}

fn fill_gpu_data(particles: &[ParticleState], out: &mut Vec<f32>) {
    out.clear();
    out.reserve(particles.len() * PARTICLE_STRIDE_FLOATS as usize);
    for particle in particles {
        out.extend_from_slice(&[particle.pos[0], particle.pos[1], particle.size, particle.alpha, particle.layer]);
    }
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
pub fn ParticleFlowBackground() -> Html {
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
            gl.blend_func(WebGlRenderingContext::SRC_ALPHA, WebGlRenderingContext::ONE_MINUS_SRC_ALPHA);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            resize_canvas(&canvas, &gl);

            let Some(buffer) = gl.create_buffer() else {
                log::error!("Failed to create floating background buffer");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };
            gl.bind_buffer(WebGlRenderingContext::ARRAY_BUFFER, Some(&buffer));

            let particles = Rc::new(RefCell::new(build_particles()));
            let gpu_data = Rc::new(RefCell::new(Vec::<f32>::with_capacity(
                PARTICLE_COUNT as usize * PARTICLE_STRIDE_FLOATS as usize,
            )));
            {
                let particles_borrow = particles.borrow();
                let mut gpu_data_borrow = gpu_data.borrow_mut();
                fill_gpu_data(&particles_borrow, &mut gpu_data_borrow);
                let array = Float32Array::from(gpu_data_borrow.as_slice());
                gl.buffer_data_with_array_buffer_view(
                    WebGlRenderingContext::ARRAY_BUFFER,
                    &array,
                    WebGlRenderingContext::DYNAMIC_DRAW,
                );
            }

            let a_pos_loc = gl.get_attrib_location(&program, "a_pos");
            if a_pos_loc < 0 {
                log::error!("Attribute a_pos not found");
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            gl.enable_vertex_attrib_array(a_pos_loc as u32);
            gl.vertex_attrib_pointer_with_i32(
                a_pos_loc as u32,
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
                3,
                WebGlRenderingContext::FLOAT,
                false,
                PARTICLE_STRIDE_BYTES,
                8,
            );

            let raf_id = Rc::new(RefCell::new(None::<i32>));
            let last_ts = Rc::new(RefCell::new(None::<f64>));
            let sim_time = Rc::new(RefCell::new(0.0_f32));
            let animation = Rc::new(RefCell::new(None::<Closure<dyn FnMut(f64)>>));

            let gl_anim = gl.clone();
            let buffer_anim = buffer.clone();
            let animation_ref = animation.clone();
            let raf_id_ref = raf_id.clone();
            let last_ts_ref = last_ts.clone();
            let sim_time_ref = sim_time.clone();
            let particles_ref = particles.clone();
            let gpu_data_ref = gpu_data.clone();

            *animation.borrow_mut() = Some(Closure::wrap(Box::new(move |timestamp: f64| {
                let dt = {
                    let mut last = last_ts_ref.borrow_mut();
                    let dt = match *last {
                        Some(prev) => ((timestamp - prev) / 1000.0) as f32,
                        None => 1.0 / 60.0,
                    };
                    *last = Some(timestamp);
                    dt.clamp(1.0 / 240.0, 1.0 / 24.0)
                };

                let time = {
                    let mut t = sim_time_ref.borrow_mut();
                    *t += dt;
                    *t
                };

                {
                    let mut particles = particles_ref.borrow_mut();
                    update_particles(&mut particles, dt, time);
                    let mut gpu_data = gpu_data_ref.borrow_mut();
                    fill_gpu_data(&particles, &mut gpu_data);
                    let array = Float32Array::from(gpu_data.as_slice());
                    gl_anim.bind_buffer(WebGlRenderingContext::ARRAY_BUFFER, Some(&buffer_anim));
                    gl_anim.buffer_data_with_array_buffer_view(
                        WebGlRenderingContext::ARRAY_BUFFER,
                        &array,
                        WebGlRenderingContext::DYNAMIC_DRAW,
                    );
                }

                gl_anim.clear(WebGlRenderingContext::COLOR_BUFFER_BIT);
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
                    let _ = win.remove_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
                }
                *animation.borrow_mut() = None;
            }) as Box<dyn FnOnce()>
        });
    }

    html! { <canvas class="tp__floating-background" ref={canvas_ref} aria-hidden="true" /> }
}

fn compile_shader(gl: &WebGlRenderingContext, shader_type: u32, source: &str) -> Result<WebGlShader, String> {
    let shader = gl.create_shader(shader_type).ok_or_else(|| String::from("Failed to create shader"))?;

    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);

    let status = gl.get_shader_parameter(&shader, WebGlRenderingContext::COMPILE_STATUS).as_bool().unwrap_or(false);

    if status {
        Ok(shader)
    } else {
        Err(gl.get_shader_info_log(&shader).unwrap_or_else(|| String::from("Unknown shader error")))
    }
}

fn link_program(gl: &WebGlRenderingContext, vert_source: &str, frag_source: &str) -> Result<WebGlProgram, String> {
    let vertex_shader = compile_shader(gl, WebGlRenderingContext::VERTEX_SHADER, vert_source)?;
    let fragment_shader = compile_shader(gl, WebGlRenderingContext::FRAGMENT_SHADER, frag_source)?;

    let program = gl.create_program().ok_or_else(|| String::from("Failed to create program"))?;

    gl.attach_shader(&program, &vertex_shader);
    gl.attach_shader(&program, &fragment_shader);
    gl.link_program(&program);

    let status = gl.get_program_parameter(&program, WebGlRenderingContext::LINK_STATUS).as_bool().unwrap_or(false);

    if status {
        Ok(program)
    } else {
        Err(gl.get_program_info_log(&program).unwrap_or_else(|| String::from("Unknown link error")))
    }
}
