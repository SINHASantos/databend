// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;
use std::fmt::Formatter;
use std::io;
use std::time::Duration;

use databend_common_meta_types::raft_types::Entry;
use databend_common_meta_types::raft_types::StorageError;
use databend_common_meta_types::snapshot_db::DB;
use databend_common_meta_types::sys_data::SysData;
use databend_common_meta_types::AppliedState;
use log::info;
use openraft::entry::RaftEntry;
use state_machine_api::SeqV;
use state_machine_api::StateMachineApi;

use crate::applier::Applier;
use crate::leveled_store::leveled_map::compactor::Compactor;
use crate::leveled_store::leveled_map::compactor_acquirer::CompactorAcquirer;
use crate::leveled_store::leveled_map::compactor_acquirer::CompactorPermit;
use crate::leveled_store::leveled_map::LeveledMap;
use crate::leveled_store::sys_data_api::SysDataApiRO;
use crate::sm_v003::sm_v003_kv_api::SMV003KVApi;

type OnChange = Box<dyn Fn((String, Option<SeqV>, Option<SeqV>)) + Send + Sync>;

#[derive(Default)]
pub struct SMV003 {
    levels: LeveledMap,

    /// Since when to start cleaning expired keys.
    cleanup_start_time_ms: Duration,

    /// Callback when a change is applied to state machine
    pub(crate) on_change_applied: Option<OnChange>,
}

impl fmt::Debug for SMV003 {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SMV003")
            .field("levels", &self.levels)
            .field(
                "on_change_applied",
                &self.on_change_applied.as_ref().map(|_x| "is_set"),
            )
            .finish()
    }
}

impl StateMachineApi<SysData> for SMV003 {
    type UserMap = LeveledMap;

    fn user_map(&self) -> &Self::UserMap {
        &self.levels
    }

    fn user_map_mut(&mut self) -> &mut Self::UserMap {
        &mut self.levels
    }

    type ExpireMap = LeveledMap;

    fn expire_map(&self) -> &Self::ExpireMap {
        &self.levels
    }

    fn expire_map_mut(&mut self) -> &mut Self::ExpireMap {
        &mut self.levels
    }

    fn cleanup_start_timestamp(&self) -> Duration {
        self.cleanup_start_time_ms
    }

    fn set_cleanup_start_timestamp(&mut self, timestamp: Duration) {
        self.cleanup_start_time_ms = timestamp;
    }

    fn sys_data_mut(&mut self) -> &mut SysData {
        self.levels.sys_data_mut()
    }

    fn on_change_applied(&mut self, change: (String, Option<SeqV>, Option<SeqV>)) {
        let Some(on_change_applied) = &self.on_change_applied else {
            // No subscribers, do nothing.
            return;
        };
        (*on_change_applied)(change);
    }
}

impl SMV003 {
    /// Return a mutable reference to the map that stores app data.
    pub(in crate::sm_v003) fn map_mut(&mut self) -> &mut LeveledMap {
        &mut self.levels
    }
}

impl SMV003 {
    pub fn kv_api(&self) -> SMV003KVApi {
        SMV003KVApi { sm: self }
    }

    /// Install and replace state machine with the content of a snapshot.
    pub async fn install_snapshot_v003(&mut self, db: DB) -> Result<(), io::Error> {
        let data_size = db.inner().file_size();
        let sys_data = db.sys_data().clone();

        info!(
            "SMV003::install_snapshot: data_size: {}; sys_data: {:?}",
            data_size, sys_data
        );

        // Do not skip install if both self.last_applied and db.last_applied are None.
        //
        // The snapshot may contain data when its last_applied is None,
        // when importing data with metactl:
        // The snapshot is empty but contains Nodes data that are manually added.
        //
        // See: `databend_metactl::import`
        let my_last_applied = *self.sys_data_ref().last_applied_ref();
        #[allow(clippy::collapsible_if)]
        if my_last_applied.is_some() {
            if &my_last_applied >= sys_data.last_applied_ref() {
                info!(
                    "SMV003 try to install a smaller snapshot({:?}), ignored, my last applied: {:?}",
                    sys_data.last_applied_ref(),
                    self.sys_data_ref().last_applied_ref()
                );
                return Ok(());
            }
        }

        self.levels.clear();
        let levels = self.levels_mut();
        *levels.sys_data_mut() = sys_data;
        *levels.persisted_mut() = Some(db);
        Ok(())
    }

    pub fn get_snapshot(&self) -> Option<DB> {
        self.levels.persisted().cloned()
    }

    #[allow(dead_code)]
    pub(crate) fn new_applier(&mut self) -> Applier<'_, Self> {
        Applier::new(self)
    }

    pub async fn apply_entries(
        &mut self,
        entries: impl IntoIterator<Item = Entry>,
    ) -> Result<Vec<AppliedState>, StorageError> {
        let mut applier = Applier::new(self);

        let mut res = vec![];

        for ent in entries.into_iter() {
            let log_id = ent.log_id();
            let r = applier
                .apply(&ent)
                .await
                .map_err(|e| StorageError::apply(log_id, &e))?;
            res.push(r);
        }
        Ok(res)
    }

    pub fn sys_data_ref(&self) -> &SysData {
        self.levels.writable_ref().sys_data_ref()
    }

    pub fn sys_data_mut(&mut self) -> &mut SysData {
        self.levels.writable_mut().sys_data_mut()
    }

    pub fn into_levels(self) -> LeveledMap {
        self.levels
    }

    pub fn levels(&self) -> &LeveledMap {
        &self.levels
    }

    pub fn levels_mut(&mut self) -> &mut LeveledMap {
        self.map_mut()
    }

    pub fn set_on_change_applied(&mut self, on_change_applied: OnChange) {
        self.on_change_applied = Some(on_change_applied);
    }

    pub fn freeze_writable(&mut self) {
        self.levels.freeze_writable();
    }

    /// A shortcut
    pub async fn acquire_compactor(&self) -> Compactor {
        let permit = self.new_compactor_acquirer().acquire().await;
        self.new_compactor(permit)
    }

    pub fn new_compactor_acquirer(&self) -> CompactorAcquirer {
        self.levels.new_compactor_acquirer()
    }

    pub fn new_compactor(&self, permit: CompactorPermit) -> Compactor {
        self.levels.new_compactor(permit)
    }

    /// Replace all the state machine data with the given one.
    /// The input is a multi-level data.
    pub fn replace(&mut self, level: LeveledMap) {
        let applied = self.map_mut().writable_ref().last_applied_ref();
        let new_applied = level.writable_ref().last_applied_ref();

        assert!(
            new_applied >= applied,
            "the state machine({:?}) can not be replaced with an older one({:?})",
            applied,
            new_applied
        );

        self.levels = level;
    }
}
