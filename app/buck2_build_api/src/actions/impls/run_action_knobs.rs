/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use dice::UserComputationData;
use dupe::Dupe;

/// Knobs controlling how RunAction works.
#[derive(Copy, Clone, Dupe, Default)]
pub struct RunActionKnobs {
    /// Process dep files as they are generated.
    pub eager_dep_files: bool,

    /// Hash all commands using the same mechanism as dep files. This allows us to skip
    /// re-executing commands if their inputs and outputs haven't changed.
    pub hash_all_commands: bool,

    /// Whether to try reading from the action output cache (in buck-out/*/offline-cache)
    /// for network actions (download_file, cas_artifact). Used to support offline
    /// builds.
    pub use_network_action_output_cache: bool,

    /// Whether to enforce timeouts when running things on RE.
    pub enforce_re_timeouts: bool,
}

pub trait HasRunActionKnobs {
    fn set_run_action_knobs(&mut self, knobs: RunActionKnobs);

    fn get_run_action_knobs(&self) -> RunActionKnobs;
}

impl HasRunActionKnobs for UserComputationData {
    fn set_run_action_knobs(&mut self, knobs: RunActionKnobs) {
        self.data.set(knobs);
    }

    fn get_run_action_knobs(&self) -> RunActionKnobs {
        *self
            .data
            .get::<RunActionKnobs>()
            .expect("RunActionKnobs should be set")
    }
}
