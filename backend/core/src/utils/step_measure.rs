use log::{debug, log_enabled, Level};
use std::time::{Duration, Instant};

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    let secs = duration.as_secs();
    let mins = secs / 60;
    let secs_rem = secs % 60;
    let millis_rem = duration.subsec_millis();

    if millis < 1_000 {
        format!("{millis} ms")
    } else if secs < 60 {
        format!("{secs}.{millis_rem:03} s")
    } else {
        format!("{mins}:{secs_rem:02}.{millis_rem:03} min")
    }
}

pub type StepMeasureCallback = Box<dyn Fn(&str, &str) + 'static + Send>;

pub struct StepMeasure {
    enabled: bool,
    name: String,
    start: Instant,
    step_start: Instant,
    callback: StepMeasureCallback,
}

impl StepMeasure {
    pub fn new<F>(name: &str, cb: F) -> Self
    where
        F: Fn(&str, &str) + 'static + Send,
    {
        Self {
            enabled: log_enabled!(Level::Debug),
            name: name.to_owned(),
            start: Instant::now(),
            step_start: Instant::now(),
            callback: Box::new(cb),
        }
    }

    pub fn broadcast(&self, step: &str, msg: &str) {
        (self.callback)(step, msg);
    }

    pub fn tick(&mut self, step: &str) {
        if self.enabled {
            let msg = format!("{}: processed {step} in {}", self.name, format_duration(self.step_start.elapsed()));
            debug!("{msg}");
            self.broadcast(&self.name, &msg);
            self.step_start = Instant::now();
        }
    }

    pub fn stop(&mut self, step: &str) {
        if self.enabled {
            if step.is_empty() {
                debug!("{}: finished in {}", self.name, format_duration(self.start.elapsed()));
            } else {
                let msg = format!("{}: processed {step} in {}", self.name, format_duration(self.step_start.elapsed()));
                let fmsg = format!("{}: finished in {}", self.name, format_duration(self.start.elapsed()));
                debug!("{msg}");
                debug!("{fmsg}");
                self.broadcast(&self.name, &msg);
                self.broadcast(&self.name, &fmsg);
            }
            self.enabled = false;
        }
    }
}

impl Drop for StepMeasure {
    fn drop(&mut self) {
        self.stop("");
    }
}
