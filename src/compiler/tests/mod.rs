//! Compiler unit tests, one file per lowering topic. `super::*` is the `compiler` module.

use super::*;

mod assert_lowering;
mod back_edge;
mod capture_layout;
mod capture_prepass;
mod carrier_lowering;
mod file_id;
mod recover_nursery_prescan;
mod std_reach;
