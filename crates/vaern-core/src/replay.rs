//! Replay of recorded runs. [`plan`] reads a sealed run's journal into what
//! a replay needs.

pub mod plan;

pub use plan::{Plan, RecordedEffect};
