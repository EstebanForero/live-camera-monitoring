//! Movement alerts from the picture itself.
//!
//! The C200 does not offer a reliable local event feed, so this compares a
//! small grayscale copy of the live frame with the previous one. A light
//! switching, the camera turning, or someone walking through the shot all
//! count. Alerts pause while a pan or tilt command is in flight.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const GRID_W: usize = 80;
const GRID_H: usize = 45;

/// A changed patch, in fractions of the picture (origin at the top left).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MotionRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub struct MotionWatch {
    notify: AtomicBool,
    borders: AtomicBool,
    sensitivity: AtomicU8,
    name: Mutex<String>,
    previous: Mutex<Option<Vec<u8>>>,
    last_sample: Mutex<Option<Instant>>,
    last_notify: Mutex<Option<Instant>>,
    suppress_until: Mutex<Option<Instant>>,
    active_until: Mutex<Option<Instant>>,
    regions_until: Mutex<Option<Instant>>,
    regions: Mutex<Vec<MotionRegion>>,
    streak: AtomicU32,
}

impl MotionWatch {
    pub fn new() -> Self {
        Self {
            notify: AtomicBool::new(true),
            borders: AtomicBool::new(false),
            sensitivity: AtomicU8::new(5),
            name: Mutex::new("Tapo C200".into()),
            previous: Mutex::new(None),
            last_sample: Mutex::new(None),
            last_notify: Mutex::new(None),
            suppress_until: Mutex::new(None),
            active_until: Mutex::new(None),
            regions_until: Mutex::new(None),
            regions: Mutex::new(Vec::new()),
            streak: AtomicU32::new(0),
        }
    }

    pub fn configure(&self, notify: bool, borders: bool, sensitivity: u8, name: &str) {
        self.notify.store(notify, Ordering::Relaxed);
        self.borders.store(borders, Ordering::Relaxed);
        self.sensitivity
            .store(sensitivity.clamp(1, 10), Ordering::Relaxed);
        *self.name.lock().expect("name") = name.to_string();
        if !notify {
            self.streak.store(0, Ordering::Relaxed);
        }
        if !borders {
            *self.regions_until.lock().expect("hold") = None;
            self.regions.lock().expect("regions").clear();
        }
        if !notify && !borders {
            *self.active_until.lock().expect("active") = None;
        }
    }

    pub fn suppress_for(&self, duration: Duration) {
        *self.suppress_until.lock().expect("suppress") = Some(Instant::now() + duration);
        self.streak.store(0, Ordering::Relaxed);
        *self.regions_until.lock().expect("hold") = None;
        self.regions.lock().expect("regions").clear();
    }

    pub fn notifies(&self) -> bool {
        self.notify.load(Ordering::Relaxed)
    }

    pub fn borders(&self) -> bool {
        self.borders.load(Ordering::Relaxed)
    }

    pub fn active(&self) -> bool {
        self.active_until
            .lock()
            .expect("active")
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    /// Boxes from the latest detection. They linger briefly so the outline
    /// does not blink off between samples.
    pub fn regions(&self) -> Vec<MotionRegion> {
        if !self.borders() {
            return Vec::new();
        }
        let visible = self
            .regions_until
            .lock()
            .expect("hold")
            .is_some_and(|until| Instant::now() < until);
        if !visible {
            return Vec::new();
        }
        self.regions.lock().expect("regions").clone()
    }

    pub fn observe(&self, jpeg: &[u8]) {
        let wants_notify = self.notifies();
        let draw_borders = self.borders();
        if !wants_notify && !draw_borders {
            return;
        }
        let now = Instant::now();
        if self
            .suppress_until
            .lock()
            .expect("suppress")
            .is_some_and(|until| now < until)
        {
            return;
        }

        let gap = {
            let mut last = self.last_sample.lock().expect("sample");
            let gap = last.map(|t| now.duration_since(t));
            if gap.is_some_and(|age| age < Duration::from_millis(160)) {
                return;
            }
            *last = Some(now);
            gap
        };

        let Some(luma) = luma_small(jpeg) else {
            return;
        };
        let mut previous = self.previous.lock().expect("previous");
        let Some(old) = previous.clone() else {
            *previous = Some(luma);
            return;
        };
        // A reconnect leaves a gap. Take a fresh baseline instead of alerting
        // on the first picture back.
        if gap.is_some_and(|age| age > Duration::from_secs(3)) {
            *previous = Some(luma);
            self.streak.store(0, Ordering::Relaxed);
            return;
        }
        let sensitivity = self.sensitivity.load(Ordering::Relaxed);
        let cell = cell_threshold(sensitivity);
        let boxes = motion_boxes(&old, &luma, cell);
        let score = mean_abs_diff(&old, &luma);
        let changed = changed_count(&old, &luma, cell) as f32 / (GRID_W * GRID_H) as f32;
        // A light switching touches most of the picture. Take that as the new
        // background. A person does not, so the background stays put and the
        // outline can follow them from sample to sample.
        if changed > 0.45 {
            *previous = Some(luma);
        } else {
            let mut base = old;
            follow_background(&mut base, &luma, cell);
            *previous = Some(base);
        }
        drop(previous);

        if !boxes.is_empty() {
            if draw_borders {
                *self.regions.lock().expect("regions") = boxes.clone();
                *self.regions_until.lock().expect("hold") =
                    Some(now + Duration::from_millis(1400));
            }
            *self.active_until.lock().expect("active") = Some(now + Duration::from_secs(2));
        }

        // Outlines and desktop alerts are separate. Alerts still wait for two
        // agreeing samples so a single flicker does not notify.
        if !wants_notify {
            return;
        }
        let alert = score >= threshold(sensitivity) || !boxes.is_empty();
        if !alert {
            self.streak.store(0, Ordering::Relaxed);
            return;
        }
        let streak = self.streak.fetch_add(1, Ordering::Relaxed) + 1;
        if streak < 2 {
            return;
        }
        self.streak.store(0, Ordering::Relaxed);

        let mut last_notify = self.last_notify.lock().expect("notify");
        if last_notify.is_some_and(|t| now.duration_since(t) < Duration::from_secs(20)) {
            return;
        }
        *last_notify = Some(now);
        drop(last_notify);

        let name = self.name.lock().expect("name").clone();
        thread::spawn(move || notify(&name));
    }
}

fn threshold(sensitivity: u8) -> f32 {
    28.0 - f32::from(sensitivity.clamp(1, 10)) * 2.0
}

fn cell_threshold(sensitivity: u8) -> u8 {
    // 1 → 24, 5 → 16, 10 → 12. This is per cell, so a small subject still
    // outlines even when the rest of the picture is still.
    (26 - i16::from(sensitivity.clamp(1, 10)) * 2).clamp(12, 24) as u8
}

fn changed_count(old: &[u8], new: &[u8], cell_threshold: u8) -> usize {
    old.iter()
        .zip(new)
        .filter(|(a, b)| a.abs_diff(**b) >= cell_threshold)
        .count()
}

/// Unchanged cells track the live picture. Changed cells creep toward it, so
/// someone who stops is absorbed and someone who keeps moving stays outlined.
fn follow_background(baseline: &mut [u8], current: &[u8], cell_threshold: u8) {
    for (base, sample) in baseline.iter_mut().zip(current) {
        if base.abs_diff(*sample) < cell_threshold {
            *base = *sample;
        } else {
            *base = ((*base as u16 * 31 + *sample as u16) / 32) as u8;
        }
    }
}

/// Bounding boxes of changed patches on the 80×45 sample.
fn motion_boxes(old: &[u8], new: &[u8], cell_threshold: u8) -> Vec<MotionRegion> {
    let cells = GRID_W * GRID_H;
    if old.len() != cells || new.len() != cells {
        return Vec::new();
    }
    let changed: Vec<bool> = old
        .iter()
        .zip(new)
        .map(|(a, b)| a.abs_diff(*b) >= cell_threshold)
        .collect();
    let mut seen = vec![false; cells];
    let mut boxes = Vec::new();
    for start in 0..cells {
        if !changed[start] || seen[start] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let mut min_x = start % GRID_W;
        let mut max_x = min_x;
        let mut min_y = start / GRID_W;
        let mut max_y = min_y;
        let mut count = 0usize;
        while let Some(index) = stack.pop() {
            let x = index % GRID_W;
            let y = index / GRID_W;
            count += 1;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            let neighbors = [
                (x.wrapping_sub(1), y, x > 0),
                (x + 1, y, x + 1 < GRID_W),
                (x, y.wrapping_sub(1), y > 0),
                (x, y + 1, y + 1 < GRID_H),
                (x.wrapping_sub(1), y.wrapping_sub(1), x > 0 && y > 0),
                (x + 1, y.wrapping_sub(1), x + 1 < GRID_W && y > 0),
                (x.wrapping_sub(1), y + 1, x > 0 && y + 1 < GRID_H),
                (x + 1, y + 1, x + 1 < GRID_W && y + 1 < GRID_H),
            ];
            for (nx, ny, ok) in neighbors {
                if !ok {
                    continue;
                }
                let next = ny * GRID_W + nx;
                if changed[next] && !seen[next] {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
        // Skip single-pixel compression specks. Five cells is a small subject.
        if count >= 5 {
            boxes.push(padded_box(min_x, min_y, max_x, max_y));
        }
    }
    if boxes.is_empty() {
        let mut min_x = GRID_W;
        let mut max_x = 0usize;
        let mut min_y = GRID_H;
        let mut max_y = 0usize;
        let mut count = 0usize;
        for (index, on) in changed.iter().copied().enumerate() {
            if !on {
                continue;
            }
            count += 1;
            let x = index % GRID_W;
            let y = index / GRID_W;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        if count >= 12 {
            boxes.push(padded_box(min_x, min_y, max_x, max_y));
        }
    }
    boxes.sort_by(|a, b| {
        (b.w * b.h)
            .partial_cmp(&(a.w * a.h))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    boxes.truncate(6);
    boxes
}

fn padded_box(min_x: usize, min_y: usize, max_x: usize, max_y: usize) -> MotionRegion {
    let x0 = min_x.saturating_sub(1);
    let y0 = min_y.saturating_sub(1);
    let x1 = (max_x + 1).min(GRID_W - 1);
    let y1 = (max_y + 1).min(GRID_H - 1);
    MotionRegion {
        x: x0 as f32 / GRID_W as f32,
        y: y0 as f32 / GRID_H as f32,
        w: (x1 - x0 + 1) as f32 / GRID_W as f32,
        h: (y1 - y0 + 1) as f32 / GRID_H as f32,
    }
}

fn mean_abs_diff(left: &[u8], right: &[u8]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let sum: u64 = left
        .iter()
        .zip(right)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    sum as f32 / left.len() as f32
}

fn luma_small(jpeg: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(jpeg).ok()?;
    let small = image.resize_exact(80, 45, image::imageops::FilterType::Triangle);
    Some(small.to_luma8().into_raw())
}

fn notify(name: &str) {
    if let Err(err) = notify_rust::Notification::new()
        .summary("Movement at the camera")
        .body(&format!("{name} saw something change in the picture."))
        .appname("Camera Live")
        .timeout(notify_rust::Timeout::Milliseconds(7000))
        .show()
    {
        log::warn!("could not show a movement notification: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_frame_scores_zero_and_a_change_scores_high() {
        let still = vec![10u8; 16];
        assert!(mean_abs_diff(&still, &still) < 0.1);
        let mut moved = still.clone();
        for pixel in moved.iter_mut().take(8) {
            *pixel = 200;
        }
        assert!(mean_abs_diff(&still, &moved) > 40.0);
    }

    #[test]
    fn a_changed_block_becomes_one_box_and_a_still_frame_does_not() {
        let cells = GRID_W * GRID_H;
        let old = vec![20u8; cells];
        assert!(motion_boxes(&old, &old, 28).is_empty());

        let mut changed = old.clone();
        for y in 10..22 {
            for x in 30..48 {
                changed[y * GRID_W + x] = 200;
            }
        }
        let boxes = motion_boxes(&old, &changed, 28);
        assert_eq!(boxes.len(), 1);
        let box_ = &boxes[0];
        assert!(box_.x < 30.0 / GRID_W as f32);
        assert!(box_.y < 10.0 / GRID_H as f32);
        assert!(box_.x + box_.w > 48.0 / GRID_W as f32);
        assert!(box_.y + box_.h > 22.0 / GRID_H as f32);
        assert!(box_.x >= 0.0 && box_.y >= 0.0);
        assert!(box_.x + box_.w <= 1.0 + f32::EPSILON);
        assert!(box_.y + box_.h <= 1.0 + f32::EPSILON);
    }

    #[test]
    fn a_small_patch_still_outlines_when_the_frame_average_barely_moves() {
        let cells = GRID_W * GRID_H;
        let old = vec![30u8; cells];
        let mut changed = old.clone();
        for y in 20..26 {
            for x in 40..46 {
                changed[y * GRID_W + x] = 90;
            }
        }
        let score = mean_abs_diff(&old, &changed);
        assert!(score < threshold(5), "score {score} should stay under the alert average");
        let boxes = motion_boxes(&old, &changed, cell_threshold(5));
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].w < 0.25 && boxes[0].h < 0.35);
    }
}
