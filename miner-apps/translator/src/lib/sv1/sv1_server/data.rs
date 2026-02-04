use std::collections::HashMap;
use stratum_apps::{
    stratum_core::{bitcoin::Target, mining_sv2::SetNewPrevHash, sv1_api::server_to_client},
    utils::types::{ChannelId, DownstreamId, Hashrate},
};

use crate::{is_aggregated, is_non_aggregated};

#[derive(Debug, Clone)]
pub struct PendingTargetUpdate {
    pub downstream_id: DownstreamId,
    pub new_target: Target,
    pub new_hashrate: Hashrate,
}

#[derive(Debug)]
pub struct Sv1ServerData {
    /// HashMap to store the SetNewPrevHash for each channel
    /// Used in both aggregated and non-aggregated mode
    pub prevhashes: HashMap<ChannelId, SetNewPrevHash<'static>>,
    /// Job storage for aggregated mode - all Sv1 downstreams share the same jobs
    pub aggregated_valid_jobs: Option<Vec<server_to_client::Notify<'static>>>,
    /// Job storage for non-aggregated mode - each Sv1 downstream has its own jobs
    pub non_aggregated_valid_jobs:
        Option<HashMap<ChannelId, Vec<server_to_client::Notify<'static>>>>,
    /// Tracks pending target updates that are waiting for SetTarget response from upstream
    pub pending_target_updates: Vec<PendingTargetUpdate>,
    /// The initial target used when opening channels - used when no downstreams remain
    pub initial_target: Option<Target>,
}

/// Delimiter used to separate original job ID from keepalive mutation counter.
/// Format: `{original_job_id}#{counter}`
pub const KEEPALIVE_JOB_ID_DELIMITER: char = '#';

impl Sv1ServerData {
    pub fn new() -> Self {
        Self {
            prevhashes: HashMap::new(),
            aggregated_valid_jobs: is_aggregated().then(Vec::new),
            non_aggregated_valid_jobs: is_non_aggregated().then(HashMap::new),
            pending_target_updates: Vec::new(),
            initial_target: None,
        }
    }

    /// Gets the prevhash for a given channel.
    pub fn get_prevhash(&self, channel_id: u32) -> Option<SetNewPrevHash<'static>> {
        self.prevhashes.get(&channel_id).cloned()
    }

    /// Sets the prevhash for a given channel.
    pub fn set_prevhash(&mut self, channel_id: u32, prevhash: SetNewPrevHash<'static>) {
        self.prevhashes.insert(channel_id, prevhash);
    }

    /// Gets the last job from the jobs storage.
    /// In aggregated mode, returns the last job from the shared job list.
    /// In non-aggregated mode, returns the last job for the specified channel.
    pub fn get_last_job(
        &self,
        channel_id: Option<u32>,
    ) -> Option<server_to_client::Notify<'static>> {
        if let Some(jobs) = &self.aggregated_valid_jobs {
            return jobs.last().cloned();
        }
        let channel_jobs = self.non_aggregated_valid_jobs.as_ref()?;
        let ch_id = channel_id?;
        channel_jobs.get(&ch_id)?.last().cloned()
    }

    /// Gets the original upstream job by its job_id.
    /// This is used to find the base time for keepalive time capping.
    pub fn get_original_job(
        &self,
        job_id: &str,
        channel_id: Option<u32>,
    ) -> Option<server_to_client::Notify<'static>> {
        if let Some(jobs) = &self.aggregated_valid_jobs {
            return jobs.iter().find(|j| j.job_id == job_id).cloned();
        }
        let channel_jobs = self.non_aggregated_valid_jobs.as_ref()?;
        let ch_id = channel_id?;
        channel_jobs
            .get(&ch_id)?
            .iter()
            .find(|j| j.job_id == job_id)
            .cloned()
    }
}

impl Default for Sv1ServerData {
    fn default() -> Self {
        Self::new()
    }
}
