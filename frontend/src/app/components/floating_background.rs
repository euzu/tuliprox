use gloo_timers::callback::Interval;
use gloo_utils::window;
use yew::prelude::*;

#[derive(Clone, Debug, Properties, PartialEq)]
struct Star {
    x: f64,
    y: f64,
    size: f64,
    dx: f64,
    dy: f64,
    anim_delay: f64,
}

#[function_component]
pub fn FloatingBackground() -> Html {
    let stars = use_state(|| {
        let width = window().inner_width().unwrap().as_f64().unwrap_or(800.0);
        let height = window().inner_height().unwrap().as_f64().unwrap_or(600.0);
        let area = width * height;
        let num_stars = ((area / 50000.0) as usize).clamp(10, 40);
        let mut rng = fastrand::Rng::new();
        let stars: Vec<Star> = (0..num_stars)
            .map(|_| Star {
                x: rng.f64() * 100.0,
                y: rng.f64() * 100.0,
                size: 16.0 + fastrand::f64() * (5.0 - 2.0),
                dx: -0.025 + rng.f64() * 0.1,
                dy: -0.025 + rng.f64() * 0.1,
                anim_delay: rng.f64() * 8.0,
            })
            .collect();
        stars
    });

    {
        let stars = stars.clone();
        use_effect(move || {
            let interval = Interval::new(200, move || {
                stars.set(
                    stars
                        .iter()
                        .map(|s| {
                            let mut star = s.clone();
                            star.x += star.dx;
                            star.y += star.dy;
                            if star.x < 0.0 {
                                star.x = 100.0;
                            }
                            if star.x > 100.0 {
                                star.x = 0.0;
                            }
                            if star.y < 0.0 {
                                star.y = 100.0;
                            }
                            if star.y > 100.0 {
                                star.y = 0.0;
                            }
                            star
                        })
                        .collect::<Vec<Star>>(),
                );
            });
            move || drop(interval)
        });
    }

    html! {
        <div class="tp__floating-background">
            { for stars.iter().map(|s| html! {
                <div class="tp__star"
                    style={format!("width: {}px; height: {}px; left: {}%; top: {}%; animation-delay: {}s;", s.size, s.size, s.x, s.y, s.anim_delay)}
                />
            })}
        </div>
    }
}
