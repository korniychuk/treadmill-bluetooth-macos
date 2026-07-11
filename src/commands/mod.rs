//! CLI command handlers, split out of `main.rs` (задача 050).

pub(crate) mod belt;
pub(crate) mod common;
pub(crate) mod diag;
pub(crate) mod stats;
pub(crate) mod status;
pub(crate) mod zone;
pub(crate) mod zone_prompts;

pub(crate) use belt::run_control;
pub(crate) use common::refuse_if_daemon_live;
pub(crate) use diag::{
    run_connect, run_daemon, run_discover, run_fitshow_probe, run_fitshow_set, run_hr,
    run_notify_test, run_sniff,
};
pub(crate) use stats::{run_default_speed, run_stats};
pub(crate) use status::{run_doctor, run_status};
pub(crate) use zone::run_zone;
