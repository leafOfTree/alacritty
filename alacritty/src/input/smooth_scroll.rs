//! Smooth scrolling while a `ScrollUp`/`ScrollDown` binding is held.
//!
//! Scrolling is driven by a single repeating timer ticking at the monitor's refresh rate. Each
//! tick advances the scroll position based on the elapsed time. Whole lines are scrolled through
//! the terminal's display offset, while the remaining fraction of a line is rendered as a pixel
//! offset of the grid, so the content moves continuously instead of jumping line by line.

use std::time::{Duration, Instant};

use winit::keyboard::PhysicalKey;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};

use crate::event::{Event, EventType};
use crate::input::ActionContext;
use crate::scheduler::{TimerId, Topic};

/// Scroll speed right after the binding is pressed, in lines per second.
const START_SPEED: f64 = 30.;

/// Scroll speed once fully accelerated, in lines per second.
const MAX_SPEED: f64 = 120.;

/// Time until the maximum scroll speed is reached.
const RAMP_DURATION: Duration = Duration::from_millis(300);

/// Upper bound for the time between two ticks, to avoid jumps after the event loop stalled.
const MAX_TICK_DELTA: Duration = Duration::from_millis(100);

/// State of an active smooth scroll.
#[derive(Debug)]
pub struct SmoothScroll {
    /// Scroll direction, `1` scrolls up and `-1` scrolls down.
    direction: i32,

    /// Key which started the scroll; `None` when triggered by a mouse binding.
    key: Option<PhysicalKey>,

    started: Instant,
    last_tick: Instant,

    /// Fraction of a line the grid is shifted up from the current display offset, in `[0, 1)`.
    fraction: f64,

    /// Whether the binding was released and the scroll stops at the next full line.
    settling: bool,
}

impl SmoothScroll {
    fn new(direction: i32, now: Instant) -> Self {
        Self { direction, key: None, started: now, last_tick: now, fraction: 0., settling: false }
    }

    /// Number of pixels the grid is shifted up.
    pub fn pixel_offset(&self, cell_height: f32) -> f32 {
        (self.fraction as f32 * cell_height).round()
    }

    /// Lines travelled since the last tick.
    fn distance(&mut self, now: Instant) -> f64 {
        let delta = now.saturating_duration_since(self.last_tick).min(MAX_TICK_DELTA);
        self.last_tick = now;

        // Ease in from the start speed to the maximum speed.
        let elapsed = now.saturating_duration_since(self.started);
        let progress = (elapsed.as_secs_f64() / RAMP_DURATION.as_secs_f64()).min(1.);
        let eased = progress * progress * (3. - 2. * progress);
        let speed = START_SPEED + (MAX_SPEED - START_SPEED) * eased;

        speed * delta.as_secs_f64()
    }

    /// Move the scroll position by `distance` lines.
    ///
    /// Returns the number of whole lines the display offset must be scrolled by and whether the
    /// scroll is finished.
    fn step(&mut self, distance: f64, display_offset: usize, history_size: usize) -> (i32, bool) {
        let mut lines = 0;

        if self.direction > 0 {
            self.fraction -= distance;
            while self.fraction < 0. {
                if self.settling || display_offset + lines as usize >= history_size {
                    self.fraction = 0.;
                    return (lines, true);
                }

                lines += 1;
                self.fraction += 1.;
            }
        } else {
            self.fraction += distance;
            while self.fraction >= 1. {
                lines -= 1;
                self.fraction -= 1.;

                if self.settling {
                    self.fraction = 0.;
                    return (lines, true);
                }
            }

            // Content can't be shifted further up once the bottom is reached.
            if display_offset as i32 + lines <= 0 {
                self.fraction = 0.;
                return (lines, true);
            }
        }

        (lines, false)
    }
}

/// Start smooth scrolling in the given direction.
///
/// Repeated calls with the same direction keep the current scroll going, so key repeat has no
/// effect.
pub fn start<T: EventListener, A: ActionContext<T>>(ctx: &mut A, direction: i32) {
    let now = Instant::now();

    if let Some(scroll) = ctx.display().smooth_scroll.as_mut() {
        if scroll.direction != direction || scroll.settling {
            scroll.direction = direction;
            scroll.settling = false;
            scroll.started = now;
            scroll.key = None;
        }
        return;
    }

    ctx.display().smooth_scroll = Some(SmoothScroll::new(direction, now));

    let interval = ctx.display().vblank_interval();
    let window_id = ctx.window().id();
    let event = Event::new(EventType::SmoothScroll, window_id);
    let timer_id = TimerId::new(Topic::SmoothScroll, window_id);
    ctx.scheduler_mut().schedule(event, interval, true, timer_id);
}

/// Associate the active smooth scroll with the key that triggered it.
pub fn bind_key<T: EventListener, A: ActionContext<T>>(ctx: &mut A, key: PhysicalKey) {
    if let Some(scroll) = ctx.display().smooth_scroll.as_mut() {
        scroll.key.get_or_insert(key);
    }
}

/// Finish smooth scrolling at the next full line if it was started by `key`.
///
/// Passing `None` releases a scroll started by a mouse binding.
pub fn release<T: EventListener, A: ActionContext<T>>(ctx: &mut A, key: Option<PhysicalKey>) {
    let scroll = match ctx.display().smooth_scroll.as_mut() {
        Some(scroll) if scroll.key == key => scroll,
        _ => return,
    };

    if scroll.fraction == 0. {
        stop(ctx);
    } else {
        scroll.settling = true;
    }
}

/// Stop smooth scrolling immediately, snapping to the current line.
pub fn stop<T: EventListener, A: ActionContext<T>>(ctx: &mut A) {
    if ctx.display().smooth_scroll.take().is_some() {
        let timer_id = TimerId::new(Topic::SmoothScroll, ctx.window().id());
        ctx.scheduler_mut().unschedule(timer_id);
        ctx.mark_dirty();
    }
}

/// Advance the active smooth scroll.
pub fn tick<T: EventListener, A: ActionContext<T>>(ctx: &mut A) {
    let grid = ctx.terminal().grid();
    let (display_offset, history_size) = (grid.display_offset(), grid.history_size());

    let (lines, done) = match ctx.display().smooth_scroll.as_mut() {
        Some(scroll) => {
            let distance = scroll.distance(Instant::now());
            scroll.step(distance, display_offset, history_size)
        },
        None => return,
    };

    if lines != 0 {
        ctx.scroll(Scroll::Delta(lines));
    }

    if done {
        stop(ctx);
    }

    ctx.mark_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn travel(scroll: &mut SmoothScroll, start: Instant, ms: u64, step: u64) -> f64 {
        (1..=ms / step).map(|i| scroll.distance(start + Duration::from_millis(i * step))).sum()
    }

    #[test]
    fn reaches_max_speed() {
        let start = Instant::now();
        let mut scroll = SmoothScroll::new(1, start);
        travel(&mut scroll, start, RAMP_DURATION.as_millis() as u64, 12);

        // Exactly one second at the maximum speed.
        let lines = travel(&mut scroll, start + RAMP_DURATION, 1000, 10);
        assert!((lines - MAX_SPEED).abs() < 1e-6, "travelled {lines} lines");
    }

    #[test]
    fn independent_of_tick_rate() {
        let start = Instant::now();
        let mut slow = SmoothScroll::new(1, start);
        let mut fast = SmoothScroll::new(1, start);
        let slow_lines = travel(&mut slow, start, 960, 16);
        let fast_lines = travel(&mut fast, start, 960, 8);
        assert!((slow_lines - fast_lines).abs() < 0.5);
    }

    #[test]
    fn clamps_stalled_ticks() {
        let start = Instant::now();
        let mut scroll = SmoothScroll::new(1, start);
        let lines = scroll.distance(start + Duration::from_secs(5));
        assert!(lines <= MAX_SPEED * MAX_TICK_DELTA.as_secs_f64());
    }

    #[test]
    fn scroll_up_keeps_fraction() {
        let mut scroll = SmoothScroll::new(1, Instant::now());

        // Moving up a quarter line scrolls one line and shifts the grid back up by 3/4 lines.
        assert_eq!(scroll.step(0.25, 0, 100), (1, false));
        assert_eq!(scroll.fraction, 0.75);

        assert_eq!(scroll.step(0.5, 1, 100), (0, false));
        assert_eq!(scroll.fraction, 0.25);

        assert_eq!(scroll.step(1.5, 1, 100), (2, false));
        assert_eq!(scroll.fraction, 0.75);
    }

    #[test]
    fn scroll_down_keeps_fraction() {
        let mut scroll = SmoothScroll::new(-1, Instant::now());

        assert_eq!(scroll.step(0.25, 10, 100), (0, false));
        assert_eq!(scroll.fraction, 0.25);

        assert_eq!(scroll.step(1.5, 10, 100), (-1, false));
        assert_eq!(scroll.fraction, 0.75);
    }

    #[test]
    fn stops_at_top() {
        let mut scroll = SmoothScroll::new(1, Instant::now());
        assert_eq!(scroll.step(2.5, 98, 100), (2, true));
        assert_eq!(scroll.fraction, 0.);
    }

    #[test]
    fn stops_at_bottom() {
        let mut scroll = SmoothScroll::new(-1, Instant::now());
        assert_eq!(scroll.step(0.5, 0, 100), (0, true));

        let mut scroll = SmoothScroll::new(-1, Instant::now());
        assert_eq!(scroll.step(1.5, 1, 100), (-1, true));
        assert_eq!(scroll.fraction, 0.);
    }

    #[test]
    fn settles_on_next_line() {
        let mut scroll = SmoothScroll::new(1, Instant::now());
        scroll.step(0.25, 0, 100);
        scroll.settling = true;
        assert_eq!(scroll.step(0.5, 1, 100), (0, false));
        assert_eq!(scroll.step(0.5, 1, 100), (0, true));
        assert_eq!(scroll.fraction, 0.);

        let mut scroll = SmoothScroll::new(-1, Instant::now());
        scroll.step(0.25, 10, 100);
        scroll.settling = true;
        assert_eq!(scroll.step(5., 10, 100), (-1, true));
        assert_eq!(scroll.fraction, 0.);
    }
}
