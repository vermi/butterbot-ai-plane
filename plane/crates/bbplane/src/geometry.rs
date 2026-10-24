//! Where things are, from the camera alone: no extra model, so no extra time. The bearing to
//! something comes from where it sits across the frame (and the lens's field of view); its
//! distance from where it meets the floor, since the camera sits at a fixed height and tilt, or
//! for a face from how big it looks. From those, a route: how far to turn and how long to drive.
//!
//! The optics (field of view, camera height and tilt, driving speed) are kept in
//! `<data_dir>/optics.json`; `bbplane optics` measures them (see `calibrate_tilt`, `speed_from`).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// An adult face is about this wide (the face detector's box).
const FACE_WIDTH_CM: f64 = 15.0;
/// Rows this close to the horizon (degrees) are too far away to tell.
const MIN_DIP_DEG: f64 = 1.5;
const MAX_CM: f64 = 800.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Optics {
    /// Horizontal field of view of the camera, degrees.
    pub hfov_deg: f64,
    /// Height of the camera above the floor, centimetres.
    pub height_cm: f64,
    /// How far the camera looks down from level, degrees (negative: up).
    pub tilt_down_deg: f64,
    /// How far right of straight ahead the camera points, degrees (negative: left).
    pub yaw_deg: f64,
    /// Forward speed at motor speed 50 and 100, centimetres a second. It is far from proportional
    /// (the tracks run nearly as fast at half power); in between it is interpolated.
    pub cm_per_s_at_half: f64,
    pub cm_per_s_at_full: f64,
    /// Turns are timed: this scales them to what it has seen them really do (learned while it
    /// turns toward things, see `Robot::approach`).
    pub turn_gain: f64,
}

impl Default for Optics {
    fn default() -> Self {
        // Measured on a ButterBot (OV3660): lens centre 9 cm up; two things 1 m ahead, 3.8 cm left
        // and 20.3 cm right; 4 cm in 1.5 s at half power, 4.5 cm at full.
        Optics {
            hfov_deg: 72.8,
            height_cm: 9.0,
            tilt_down_deg: -1.07,
            yaw_deg: 0.92,
            cm_per_s_at_half: 2.7,
            cm_per_s_at_full: 3.0,
            turn_gain: 1.0,
        }
    }
}

impl Optics {
    pub fn load(data_dir: &Path) -> Optics {
        std::fs::read(data_dir.join("optics.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::write(
            data_dir.join("optics.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        Ok(())
    }

    /// Focal length in pixels for a frame `width` wide (square pixels).
    fn focal(&self, width: f64) -> f64 {
        (width / 2.0) / (self.hfov_deg.to_radians() / 2.0).tan()
    }

    /// Degrees to turn to face column `x` (positive: right).
    pub fn bearing(&self, x: f64, width: f64) -> f64 {
        self.off_axis(x, width) + self.yaw_deg
    }

    /// How far from the camera's own axis column `x` looks, degrees.
    fn off_axis(&self, x: f64, width: f64) -> f64 {
        ((x - width / 2.0) / self.focal(width)).atan().to_degrees()
    }

    /// How far below level row `y` looks, degrees.
    fn dip(&self, y: f64, width: f64, height: f64) -> f64 {
        self.tilt_down_deg + ((y - height / 2.0) / self.focal(width)).atan().to_degrees()
    }

    /// How far ahead (along the camera's axis) row `y` meets the floor, if it is not too far.
    pub fn floor_distance(&self, y: f64, width: f64, height: f64) -> Option<f64> {
        let dip = self.dip(y, width, height);
        (dip >= MIN_DIP_DEG)
            .then(|| self.height_cm / dip.to_radians().tan())
            .filter(|d| *d <= MAX_CM)
    }

    /// Where a detector's box (x, y, w, h in pixels of a `width` x `height` frame) is. `face`:
    /// judge a face by its size; anything else by where its bottom meets the floor.
    pub fn place_of_box(&self, bbox: [f32; 4], width: f64, height: f64, face: bool) -> Place {
        let [x, y, w, h] = bbox.map(f64::from);
        let bearing_deg = self.bearing(x + w / 2.0, width);
        if face {
            let d = FACE_WIDTH_CM * self.focal(width) / w.max(1.0);
            return Place {
                bearing_deg,
                distance_cm: (d <= MAX_CM).then_some(d),
                judged_by: Some(Judged::Size),
            };
        }
        let bottom = y + h;
        // Its foot is out of the frame: it is closer than where the frame's bottom row meets the floor.
        if bottom >= height - 2.0 {
            return Place {
                bearing_deg,
                distance_cm: self.floor_distance(height, width, height),
                judged_by: Some(Judged::CloserThan),
            };
        }
        // Off to the side, it is further than it is ahead.
        let aside = self.off_axis(x + w / 2.0, width).to_radians().cos();
        let distance_cm = self
            .floor_distance(bottom, width, height)
            .map(|d| d / aside);
        Place {
            bearing_deg,
            distance_cm,
            judged_by: distance_cm.map(|_| Judged::Floor),
        }
    }

    /// Where a spot (fractions across and down the frame) is, if it is on the floor.
    pub fn place_of_point(&self, fx: f64, fy: f64) -> Place {
        // Any frame size will do: only the ratio matters.
        let (width, height) = (640.0, 480.0);
        let aside = self.off_axis(fx * width, width).to_radians().cos();
        let distance_cm = self
            .floor_distance(fy * height, width, height)
            .map(|d| d / aside);
        Place {
            bearing_deg: self.bearing(fx * width, width),
            distance_cm,
            judged_by: distance_cm.map(|_| Judged::Floor),
        }
    }

    /// Forward speed at motor speed `speed` (1..100), centimetres a second. Below 20 the tracks
    /// hardly move.
    pub fn cm_per_s(&self, speed: u8) -> f64 {
        let s = f64::from(speed.clamp(1, 100));
        if s >= 50.0 {
            self.cm_per_s_at_half
                + (self.cm_per_s_at_full - self.cm_per_s_at_half) * (s - 50.0) / 50.0
        } else {
            (self.cm_per_s_at_half * (s - 20.0) / 30.0).max(0.1)
        }
    }

    /// Seconds of driving at `speed` (1..100) to cover `cm`.
    pub fn drive_secs(&self, cm: f64, speed: u8) -> f64 {
        cm.max(0.0) / self.cm_per_s(speed)
    }

    /// The tilt that puts the bottom of a box at row `y` at `distance_cm` along the floor.
    pub fn calibrate_tilt(&mut self, y: f64, width: f64, height: f64, distance_cm: f64) {
        let true_dip = (self.height_cm / distance_cm).atan().to_degrees();
        let in_frame = ((y - height / 2.0) / self.focal(width)).atan().to_degrees();
        self.tilt_down_deg = true_dip - in_frame;
    }

    /// The speed that took it from `before_cm` to `after_cm` in `secs` at `speed` (kept as the
    /// half or the full speed, whichever is nearer).
    pub fn speed_from(&mut self, before_cm: f64, after_cm: f64, secs: f64, speed: u8) -> bool {
        let covered = before_cm - after_cm;
        if covered <= 1.0 || secs <= 0.0 {
            return false;
        }
        let v = covered / secs;
        if speed >= 75 {
            self.cm_per_s_at_full = v;
        } else {
            self.cm_per_s_at_half = v;
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Judged {
    /// Where it meets the floor.
    Floor,
    /// How big a face looks.
    Size,
    /// Its bottom is out of frame: closer than this.
    CloserThan,
}

/// Where something is, seen from the robot.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Place {
    /// Degrees to turn to face it: positive right, negative left.
    pub bearing_deg: f64,
    /// Along the floor, centimetres, if it can be told.
    pub distance_cm: Option<f64>,
    pub judged_by: Option<Judged>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn optics() -> Optics {
        Optics {
            hfov_deg: 60.0,
            height_cm: 10.0,
            tilt_down_deg: 0.0,
            yaw_deg: 0.0,
            cm_per_s_at_half: 20.0,
            cm_per_s_at_full: 30.0,
            turn_gain: 1.0,
        }
    }

    #[test]
    fn the_measured_optics_put_both_things_where_they_were() {
        // The calibration shots: 640x480, bottoms at row 287, 1 m ahead.
        let o = Optics::default();
        let left = o.place_of_box([290.0, 260.0, 13.0, 27.25], 640.0, 480.0, false);
        let right = o.place_of_box([395.0, 240.0, 12.0, 47.25], 640.0, 480.0, false);
        assert!((left.bearing_deg - (-2.18)).abs() < 0.3, "{left:?}");
        assert!((right.bearing_deg - 11.49).abs() < 0.3, "{right:?}");
        assert!((left.distance_cm.unwrap() - 100.0).abs() < 5.0, "{left:?}");
        assert!(
            (right.distance_cm.unwrap() - 102.0).abs() < 5.0,
            "{right:?}"
        );
    }

    #[test]
    fn bearings_follow_the_frame() {
        let o = optics();
        assert!(o.bearing(320.0, 640.0).abs() < 1e-9, "dead ahead");
        assert!(
            (o.bearing(640.0, 640.0) - 30.0).abs() < 1e-6,
            "the right edge is half the view"
        );
        assert!((o.bearing(0.0, 640.0) + 30.0).abs() < 1e-6);
    }

    #[test]
    fn lower_in_the_frame_is_closer() {
        let o = optics();
        assert_eq!(o.floor_distance(240.0, 640.0, 480.0), None, "the horizon");
        let near = o.floor_distance(470.0, 640.0, 480.0).unwrap();
        let far = o.floor_distance(300.0, 640.0, 480.0).unwrap();
        assert!(near < far);
        // A dip of 45 degrees is as far away as the camera is high.
        let mut o = optics();
        o.tilt_down_deg = 45.0;
        assert!((o.floor_distance(240.0, 640.0, 480.0).unwrap() - 10.0).abs() < 1e-6);
    }

    #[test]
    fn calibration_puts_things_where_they_were_measured() {
        let mut o = optics();
        o.calibrate_tilt(400.0, 640.0, 480.0, 50.0);
        assert!((o.floor_distance(400.0, 640.0, 480.0).unwrap() - 50.0).abs() < 1e-6);
        assert!(o.speed_from(80.0, 50.0, 1.5, 50));
        assert!((o.cm_per_s_at_half - 20.0).abs() < 1e-9);
        assert!((o.drive_secs(40.0, 50) - 2.0).abs() < 1e-9);
        assert!(
            (o.cm_per_s(75) - 25.0).abs() < 1e-9,
            "between half and full"
        );
        assert!((o.cm_per_s(35) - 10.0).abs() < 1e-9, "slower below half");
        assert!(!o.speed_from(50.0, 52.0, 1.0, 50), "it did not get closer");
    }

    #[test]
    fn a_box_cut_off_at_the_bottom_is_closer_than_the_frame_edge() {
        let o = Optics {
            tilt_down_deg: 20.0,
            ..optics()
        };
        let p = o.place_of_box([300.0, 300.0, 40.0, 180.0], 640.0, 480.0, false);
        assert_eq!(p.judged_by, Some(Judged::CloserThan));
        let p = o.place_of_box([300.0, 300.0, 40.0, 60.0], 640.0, 480.0, false);
        assert_eq!(p.judged_by, Some(Judged::Floor));
        let face = o.place_of_box([300.0, 100.0, 64.0, 64.0], 640.0, 480.0, true);
        assert_eq!(face.judged_by, Some(Judged::Size));
        assert!(face.distance_cm.unwrap() > 50.0);
    }
}
