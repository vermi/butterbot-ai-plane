//! Summon (a button on the handheld controller): the robot comes to whoever called it. Stock
//! firmware did this with on-robot face detection, which AI mode has no room for; here the
//! plane's object detector looks for a person while the robot turns, then it drives over in short
//! hops until they fill most of its view.

use std::time::Duration;

use anyhow::Result;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::control::Robot;
use crate::objects::Seen;

/// Turn this far between looks (8 looks make a full circle).
const SCAN_STEP_DEG: f64 = 45.0;
/// The camera's horizontal field of view, roughly.
const FOV_DEG: f64 = 60.0;
/// Close enough: a person this tall in the frame (the camera sits low).
const ARRIVED_HEIGHT: f32 = 0.8;
/// At most this many hops forward.
const MAX_HOPS: u32 = 6;
const HOP_MS: u64 = 700;
const HOP_SPEED: i8 = 55;
/// How long to wait for the detector to look at a frame taken after a move.
const LOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest person in a frame analysed after `after` (None if there is none, or no fresh frame).
async fn find_person(robot: &Robot, after: Instant, stop: &CancellationToken) -> Option<Seen> {
    // The frame must be captured after the robot stopped moving: wait a moment, then for an
    // analysis newer than that.
    let since = after + Duration::from_millis(600);
    let deadline = Instant::now() + LOOK_TIMEOUT;
    while Instant::now() < deadline && !stop.is_cancelled() {
        if let Some(seen) = robot.objects.seen_after(since) {
            return seen
                .into_iter()
                .filter(|s| s.class == "person")
                .max_by(|a, b| (a.bbox[2] * a.bbox[3]).total_cmp(&(b.bbox[2] * b.bbox[3])));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Come to the person who pressed Summon. Returns what happened, for the robot to say.
pub async fn run(robot: &Robot, stop: &CancellationToken) -> Result<&'static str> {
    let mut target = None;
    for step in 0..(360.0 / SCAN_STEP_DEG) as u32 {
        if stop.is_cancelled() {
            return Ok("");
        }
        if step > 0 {
            robot.turn(SCAN_STEP_DEG, 60).await?;
        }
        if let Some(p) = find_person(robot, Instant::now(), stop).await {
            target = Some(p);
            break;
        }
    }
    let Some(mut person) = target else {
        return Ok("I can't see you anywhere.");
    };
    for _ in 0..MAX_HOPS {
        if stop.is_cancelled() {
            return Ok("");
        }
        let (w, h) = (person.frame_width as f32, person.frame_height as f32);
        if person.bbox[3] >= ARRIVED_HEIGHT * h {
            return Ok("Here I am!");
        }
        // Face them first.
        let off = (person.bbox[0] + person.bbox[2] / 2.0) / w - 0.5;
        if off.abs() > 0.12 {
            robot.turn(off as f64 * FOV_DEG, 50).await?;
        }
        let out = robot.drive_for(HOP_SPEED, HOP_SPEED, HOP_MS).await?;
        if let Some(why) = out.stopped_early {
            tracing::info!("summon: stopped ({why})");
            return Ok("Here I am!");
        }
        match find_person(robot, Instant::now(), stop).await {
            Some(p) => person = p,
            None => return Ok("Here I am, I think!"),
        }
    }
    Ok("Here I am!")
}
