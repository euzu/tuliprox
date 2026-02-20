use fastrand::Rng;
use js_sys::{Array, Float32Array};
use std::{cell::RefCell, f32::consts::TAU, rc::Rc};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{
    window, Element, Event, HtmlCanvasElement, WebGlBuffer, WebGlProgram, WebGlRenderingContext, WebGlShader,
    WebGlUniformLocation, MutationObserver, MutationObserverInit,
};
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

struct RenderRuntime {
    gl: WebGlRenderingContext,
    program: WebGlProgram,
    buffer: WebGlBuffer,
    u_accent_1: Option<WebGlUniformLocation>,
    u_accent_2: Option<WebGlUniformLocation>,
    u_alpha_boost: Option<WebGlUniformLocation>,
    u_dpr: Option<WebGlUniformLocation>,
}

const VERTEX_SHADER: &str = r#"
attribute vec2 a_pos;
attribute vec3 a_meta; // size, alpha, layer
uniform float u_dpr;

varying float v_alpha;
varying float v_layer;

void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    gl_PointSize = a_meta.x * u_dpr * (1.0 + a_meta.z * 0.56);
    v_alpha = a_meta.y;
    v_layer = a_meta.z;
}
"#;

const FRAGMENT_SHADER: &str = r#"
precision highp float;

uniform vec3 u_accent1;
uniform vec3 u_accent2;
uniform float u_alpha_boost;

varying float v_alpha;
varying float v_layer;

void main() {
    vec2 uv = gl_PointCoord - vec2(0.5);
    float dist = length(uv);
    if (dist > 0.5) {
        discard;
    }

    float core = 1.0 - smoothstep(0.0, 0.32, dist);
    float halo = 1.0 - smoothstep(0.2, 0.5, dist);
    float alpha = (core * 0.75 + halo * 0.25) * v_alpha * u_alpha_boost;
    alpha = clamp(alpha, 0.0, 1.0);
    vec3 color = mix(u_accent1, u_accent2, clamp(v_layer * 0.35, 0.0, 1.0));
    gl_FragColor = vec4(color, alpha);
}
"#;

fn normalize(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt().max(0.00001);
    [v[0] / len, v[1] / len]
}

fn fract(v: f32) -> f32 { v - v.floor() }

fn hash1(v: f32) -> f32 { fract((v.sin() * 43_758.547).abs()) }

fn parse_rgb_component(component: &str) -> Option<f32> {
    let value = component.trim();
    if let Some(percent) = value.strip_suffix('%') {
        percent.parse::<f32>().ok().map(|v| (v / 100.0).clamp(0.0, 1.0))
    } else {
        value.parse::<f32>().ok().map(|v| (v / 255.0).clamp(0.0, 1.0))
    }
}

fn parse_css_color_to_rgb(value: &str) -> Option<[f32; 3]> {
    let color = value.trim();
    if let Some(hex) = color.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()? as f32 / 255.0;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()? as f32 / 255.0;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()? as f32 / 255.0;
                Some([r, g, b])
            }
            4 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()? as f32 / 255.0;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()? as f32 / 255.0;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()? as f32 / 255.0;
                Some([r, g, b])
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f32 / 255.0;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f32 / 255.0;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f32 / 255.0;
                Some([r, g, b])
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f32 / 255.0;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f32 / 255.0;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f32 / 255.0;
                Some([r, g, b])
            }
            _ => None,
        };
    }

    let inner = color
        .strip_prefix("rgb(")
        .and_then(|s| s.strip_suffix(')'))
        .or_else(|| color.strip_prefix("rgba(").and_then(|s| s.strip_suffix(')')))?;

    let components: Vec<&str> = inner.split(',').collect();
    if components.len() < 3 {
        return None;
    }

    let r = parse_rgb_component(components[0])?;
    let g = parse_rgb_component(components[1])?;
    let b = parse_rgb_component(components[2])?;
    Some([r, g, b])
}

fn read_theme_style() -> Option<web_sys::CssStyleDeclaration> {
    let win = window()?;
    let document = win.document()?;
    document
        .body()
        .and_then(|body| {
            body.dyn_into::<Element>()
                .ok()
                .and_then(|element| win.get_computed_style(&element).ok().flatten())
        })
        .or_else(|| {
            document
                .document_element()
                .and_then(|root| win.get_computed_style(&root).ok().flatten())
        })
}

fn read_accent_colors_from_css() -> ([f32; 3], [f32; 3]) {
    let fallback_1 = [0.45, 0.63, 0.88];
    let fallback_2 = [0.95, 0.97, 1.0];

    let Some(style) = read_theme_style() else {
        return (fallback_1, fallback_2);
    };

    let accent_1 = style
        .get_property_value("--particle-flow-background-accent-color-1")
        .ok()
        .and_then(|v| parse_css_color_to_rgb(&v))
        .unwrap_or(fallback_1);
    let accent_2 = style
        .get_property_value("--particle-flow-background-accent-color-2")
        .ok()
        .and_then(|v| parse_css_color_to_rgb(&v))
        .unwrap_or(fallback_2);

    (accent_1, accent_2)
}

fn read_alpha_boost_from_css() -> f32 {
    let fallback = 1.0_f32;
    let Some(style) = read_theme_style() else {
        return fallback;
    };

    style
        .get_property_value("--particle-flow-background-alpha-boost")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 4.0))
        .unwrap_or(fallback)
}

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

fn update_particles(particles: &mut [ParticleState], density: &mut [f32], dt: f32, time: f32) {
    let wind_dir = normalize([1.0 + 0.2 * (time * 0.04).sin(), 0.18 * (time * 0.07).sin()]);
    let wind_env = 0.38 + 0.62 * (0.5 + 0.5 * (time * 0.08 + (time * 0.023).sin() * 2.0).sin());
    let expected_density = particles.len() as f32 / (DENSITY_GRID_W * DENSITY_GRID_H) as f32;

    debug_assert_eq!(density.len(), DENSITY_GRID_W * DENSITY_GRID_H);
    density.fill(0.0);
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

        let home_pull = 0.011 + particle.layer * 0.004;
        target[0] += (particle.home[0] - particle.pos[0]) * home_pull;
        target[1] += (particle.home[1] - particle.pos[1]) * home_pull;

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

fn upload_dpr_uniform(gl: &WebGlRenderingContext, location: Option<WebGlUniformLocation>) {
    if let (Some(win), Some(location)) = (window(), location.as_ref()) {
        gl.uniform1f(Some(location), win.device_pixel_ratio().max(1.0) as f32);
    }
}

fn upload_accent_uniform(gl: &WebGlRenderingContext, location: Option<WebGlUniformLocation>, value: [f32; 3]) {
    if let Some(location) = location.as_ref() {
        gl.uniform3f(Some(location), value[0], value[1], value[2]);
    }
}

fn upload_alpha_boost_uniform(gl: &WebGlRenderingContext, location: Option<WebGlUniformLocation>) {
    if let Some(location) = location.as_ref() {
        gl.uniform1f(Some(location), read_alpha_boost_from_css());
    }
}

fn upload_theme_accent_uniforms(
    gl: &WebGlRenderingContext,
    u_accent_1: Option<WebGlUniformLocation>,
    u_accent_2: Option<WebGlUniformLocation>,
) {
    let (accent_1, accent_2) = read_accent_colors_from_css();
    upload_accent_uniform(gl, u_accent_1, accent_1);
    upload_accent_uniform(gl, u_accent_2, accent_2);
}

fn init_renderer(canvas: &HtmlCanvasElement, initial_data: &[f32]) -> Result<RenderRuntime, String> {
    let context = canvas
        .get_context("webgl")
        .ok()
        .flatten()
        .or_else(|| canvas.get_context("experimental-webgl").ok().flatten())
        .ok_or_else(|| String::from("WebGL context is not available for particle flow background"))?;

    let gl = context.dyn_into::<WebGlRenderingContext>().map_err(|_| String::from("Failed to cast WebGL context"))?;

    let program = link_program(&gl, VERTEX_SHADER, FRAGMENT_SHADER)?;
    gl.use_program(Some(&program));
    gl.enable(WebGlRenderingContext::BLEND);
    gl.blend_func(WebGlRenderingContext::SRC_ALPHA, WebGlRenderingContext::ONE_MINUS_SRC_ALPHA);
    gl.clear_color(0.0, 0.0, 0.0, 0.0);
    resize_canvas(canvas, &gl);

    let u_accent_1 = gl.get_uniform_location(&program, "u_accent1");
    let u_accent_2 = gl.get_uniform_location(&program, "u_accent2");
    let u_alpha_boost = gl.get_uniform_location(&program, "u_alpha_boost");
    let u_dpr = gl.get_uniform_location(&program, "u_dpr");
    upload_theme_accent_uniforms(&gl, u_accent_1.clone(), u_accent_2.clone());
    upload_alpha_boost_uniform(&gl, u_alpha_boost.clone());
    upload_dpr_uniform(&gl, u_dpr.clone());

    let buffer = gl.create_buffer().ok_or_else(|| String::from("Failed to create particle buffer"))?;
    gl.bind_buffer(WebGlRenderingContext::ARRAY_BUFFER, Some(&buffer));
    // SAFETY: `initial_data` is borrowed for the duration of this immediate upload call.
    let initial_array = unsafe { Float32Array::view(initial_data) };
    gl.buffer_data_with_array_buffer_view(
        WebGlRenderingContext::ARRAY_BUFFER,
        &initial_array,
        WebGlRenderingContext::DYNAMIC_DRAW,
    );

    let a_pos_loc = gl.get_attrib_location(&program, "a_pos");
    if a_pos_loc < 0 {
        return Err(String::from("Attribute a_pos not found"));
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
        return Err(String::from("Attribute a_meta not found"));
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

    Ok(RenderRuntime {
        gl,
        program,
        buffer,
        u_accent_1,
        u_accent_2,
        u_alpha_boost,
        u_dpr,
    })
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

            let particles = Rc::new(RefCell::new(build_particles()));
            let density_grid = Rc::new(RefCell::new(vec![0.0_f32; DENSITY_GRID_W * DENSITY_GRID_H]));
            let gpu_data = Rc::new(RefCell::new(Vec::<f32>::with_capacity(
                PARTICLE_COUNT as usize * PARTICLE_STRIDE_FLOATS as usize,
            )));
            {
                let particles_borrow = particles.borrow();
                let mut gpu_data_borrow = gpu_data.borrow_mut();
                fill_gpu_data(&particles_borrow, &mut gpu_data_borrow);
            }

            let initial_runtime = {
                let initial_data = gpu_data.borrow();
                match init_renderer(&canvas, initial_data.as_slice()) {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        log::error!("{err}");
                        return Box::new(|| ()) as Box<dyn FnOnce()>;
                    }
                }
            };
            let runtime_ref = Rc::new(RefCell::new(Some(initial_runtime)));

            let raf_id = Rc::new(RefCell::new(None::<i32>));
            let last_ts = Rc::new(RefCell::new(None::<f64>));
            let sim_time = Rc::new(RefCell::new(0.0_f32));
            let animation = Rc::new(RefCell::new(None::<Closure<dyn FnMut(f64)>>));

            let runtime_ref_anim = runtime_ref.clone();
            let animation_ref = animation.clone();
            let raf_id_ref = raf_id.clone();
            let last_ts_ref = last_ts.clone();
            let sim_time_ref = sim_time.clone();
            let particles_ref = particles.clone();
            let density_grid_ref = density_grid.clone();
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
                    let mut density_grid = density_grid_ref.borrow_mut();
                    update_particles(&mut particles, density_grid.as_mut_slice(), dt, time);
                    let mut gpu_data = gpu_data_ref.borrow_mut();
                    fill_gpu_data(&particles, &mut gpu_data);
                }

                if let Some(runtime) = runtime_ref_anim.borrow().as_ref() {
                    let frame_data = gpu_data_ref.borrow();
                    // SAFETY: `frame_data` is immutably borrowed and immediately consumed by WebGL upload.
                    let array = unsafe { Float32Array::view(frame_data.as_slice()) };
                    runtime.gl.bind_buffer(WebGlRenderingContext::ARRAY_BUFFER, Some(&runtime.buffer));
                    runtime.gl.buffer_sub_data_with_i32_and_array_buffer_view(
                        WebGlRenderingContext::ARRAY_BUFFER,
                        0,
                        &array,
                    );
                    runtime.gl.clear(WebGlRenderingContext::COLOR_BUFFER_BIT);
                    runtime.gl.draw_arrays(WebGlRenderingContext::POINTS, 0, PARTICLE_COUNT);
                }

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
            let runtime_ref_resize = runtime_ref.clone();
            let on_resize = Closure::<dyn FnMut()>::wrap(Box::new(move || {
                if let Some(runtime) = runtime_ref_resize.borrow().as_ref() {
                    resize_canvas(&resize_canvas_ref, &runtime.gl);
                    upload_dpr_uniform(&runtime.gl, runtime.u_dpr.clone());
                }
            }));
            let _ = win.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());

            let runtime_ref_theme = runtime_ref.clone();
            let on_theme_change =
                Closure::<dyn FnMut(Array, MutationObserver)>::wrap(Box::new(move |_records, _observer| {
                    if let Some(runtime) = runtime_ref_theme.borrow().as_ref() {
                        runtime.gl.use_program(Some(&runtime.program));
                        upload_theme_accent_uniforms(
                            &runtime.gl,
                            runtime.u_accent_1.clone(),
                            runtime.u_accent_2.clone(),
                        );
                        upload_alpha_boost_uniform(&runtime.gl, runtime.u_alpha_boost.clone());
                    }
                }));

            let theme_observer = win
                .document()
                .and_then(|doc| doc.body())
                .and_then(|body| {
                    MutationObserver::new(on_theme_change.as_ref().unchecked_ref())
                        .ok()
                        .and_then(|observer| {
                            let options = MutationObserverInit::new();
                            options.set_attributes(true);
                            observer.observe_with_options(&body, &options).ok()?;
                            Some(observer)
                        })
                });

            let runtime_ref_lost = runtime_ref.clone();
            let on_context_lost = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |event: Event| {
                event.prevent_default();
                *runtime_ref_lost.borrow_mut() = None;
            }));
            let _ =
                canvas.add_event_listener_with_callback("webglcontextlost", on_context_lost.as_ref().unchecked_ref());

            let runtime_ref_restore = runtime_ref.clone();
            let canvas_restore = canvas.clone();
            let gpu_data_restore = gpu_data.clone();
            let on_context_restored = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_event: Event| {
                let data = gpu_data_restore.borrow();
                match init_renderer(&canvas_restore, data.as_slice()) {
                    Ok(runtime) => {
                        *runtime_ref_restore.borrow_mut() = Some(runtime);
                    }
                    Err(err) => log::error!("Failed to restore WebGL context: {err}"),
                }
            }));
            let _ = canvas
                .add_event_listener_with_callback("webglcontextrestored", on_context_restored.as_ref().unchecked_ref());

            let canvas_cleanup = canvas.clone();
            Box::new(move || {
                if let Some(win) = window() {
                    if let Some(id) = *raf_id.borrow() {
                        let _ = win.cancel_animation_frame(id);
                    }
                    let _ = win.remove_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
                }
                if let Some(observer) = theme_observer.as_ref() {
                    observer.disconnect();
                }
                let _ = canvas_cleanup
                    .remove_event_listener_with_callback("webglcontextlost", on_context_lost.as_ref().unchecked_ref());
                let _ = canvas_cleanup.remove_event_listener_with_callback(
                    "webglcontextrestored",
                    on_context_restored.as_ref().unchecked_ref(),
                );
                *runtime_ref.borrow_mut() = None;
                *animation.borrow_mut() = None;
                drop(on_theme_change);
            }) as Box<dyn FnOnce()>
        });
    }

    html! { <canvas class="tp__particle-flow-background" ref={canvas_ref} aria-hidden="true" /> }
}

fn compile_shader(gl: &WebGlRenderingContext, shader_type: u32, source: &str) -> Result<WebGlShader, String> {
    let shader = gl.create_shader(shader_type).ok_or_else(|| String::from("Failed to create shader"))?;

    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);

    let status = gl.get_shader_parameter(&shader, WebGlRenderingContext::COMPILE_STATUS).as_bool().unwrap_or(false);

    if status {
        Ok(shader)
    } else {
        let error = gl.get_shader_info_log(&shader).unwrap_or_else(|| String::from("Unknown shader error"));
        gl.delete_shader(Some(&shader));
        Err(error)
    }
}

fn link_program(gl: &WebGlRenderingContext, vert_source: &str, frag_source: &str) -> Result<WebGlProgram, String> {
    let vertex_shader = compile_shader(gl, WebGlRenderingContext::VERTEX_SHADER, vert_source)?;
    let fragment_shader = match compile_shader(gl, WebGlRenderingContext::FRAGMENT_SHADER, frag_source) {
        Ok(shader) => shader,
        Err(err) => {
            gl.delete_shader(Some(&vertex_shader));
            return Err(err);
        }
    };

    let Some(program) = gl.create_program() else {
        gl.delete_shader(Some(&vertex_shader));
        gl.delete_shader(Some(&fragment_shader));
        return Err(String::from("Failed to create program"));
    };

    gl.attach_shader(&program, &vertex_shader);
    gl.attach_shader(&program, &fragment_shader);
    gl.link_program(&program);

    let status = gl.get_program_parameter(&program, WebGlRenderingContext::LINK_STATUS).as_bool().unwrap_or(false);

    if status {
        gl.detach_shader(&program, &vertex_shader);
        gl.detach_shader(&program, &fragment_shader);
        gl.delete_shader(Some(&vertex_shader));
        gl.delete_shader(Some(&fragment_shader));
        Ok(program)
    } else {
        let error = gl.get_program_info_log(&program).unwrap_or_else(|| String::from("Unknown link error"));
        gl.detach_shader(&program, &vertex_shader);
        gl.detach_shader(&program, &fragment_shader);
        gl.delete_program(Some(&program));
        gl.delete_shader(Some(&vertex_shader));
        gl.delete_shader(Some(&fragment_shader));
        Err(error)
    }
}
