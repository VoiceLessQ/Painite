//! Admission scheduler for chunk generation stages.
//!
//! Vanilla runs FEATURES, STRUCTURE_STARTS, STRUCTURE_REFERENCES and SPAWN
//! on one serial thread. The only correctness need is that no two FEATURES
//! jobs with overlapping write zones (radius 1, so chessboard distance <= 2)
//! run at once; the center-only stages just need one job per chunk. This
//! crate decides when a job may run. Threads that execute the job live on
//! the Java side and call `acquire` / `release` around the stage body.

mod grid;
mod queue;
mod scheduler;

pub use scheduler::{Job, Limits, Scheduler, Stage, Submitted, Ticket};
