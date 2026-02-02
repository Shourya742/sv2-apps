use std::{cell::RefCell, sync::atomic::AtomicBool, time::Instant};
use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        sv1_api::{
            json_rpc,
            utils::{Extranonce, HexU32Be},
        },
    },
    utils::types::{ChannelId, DownstreamId, Hashrate},
};
use tracing::debug;

use super::SubmitShareWithChannelId;

#[derive(Debug)]
pub struct DownstreamData {
    pub channel_id: Option<ChannelId>,
    pub downstream_id: DownstreamId,
    pub extranonce1: Extranonce<'static>,
    pub extranonce2_len: usize,
    pub version_rolling_mask: Option<HexU32Be>,
    pub version_rolling_min_bit: Option<HexU32Be>,
    pub last_job_version_field: Option<u32>,
    /// The raw string received from SV1 mining.authorize (e.g., "username.worker1")
    pub worker_name_from_authorize: String,
    /// The worker suffix extracted from mining.authorize for TLV extension (e.g., "worker1" from "username.worker1")
    /// If no '.' delimiter exists, contains the full authorize name.
    pub user_identity: String,
    pub target: Target,
    pub hashrate: Option<Hashrate>,
    pub cached_set_difficulty: Option<json_rpc::Message>,
    pub cached_notify: Option<json_rpc::Message>,
    pub pending_target: Option<Target>,
    pub pending_hashrate: Option<Hashrate>,
    // Flag to track if SV1 handshake is complete (subscribe + authorize)
    pub sv1_handshake_complete: AtomicBool,
    // Queue of Sv1 handshake messages received while waiting for SV2 channel to open
    pub queued_sv1_handshake_messages: Vec<json_rpc::Message>,
    // Flag to indicate we're processing queued Sv1 handshake message responses
    pub processing_queued_sv1_handshake_responses: AtomicBool,
    // Stores pending shares to be sent to the sv1_server
    pub pending_share: RefCell<Option<SubmitShareWithChannelId>>,
    // Tracks the upstream target for this downstream, used for vardiff target comparison
    pub upstream_target: Option<Target>,
    // Timestamp of when the last job was received by this downstream, used for keepalive check
    pub last_job_received_time: Option<Instant>,
}

impl DownstreamData {
    pub fn new(downstream_id: DownstreamId, target: Target, hashrate: Option<Hashrate>) -> Self {
        DownstreamData {
            channel_id: None,
            downstream_id,
            extranonce1: vec![0; 8]
                .try_into()
                .expect("8-byte extranonce is always valid"),
            extranonce2_len: 4,
            version_rolling_mask: None,
            version_rolling_min_bit: None,
            last_job_version_field: None,
            worker_name_from_authorize: String::new(),
            user_identity: String::new(),
            target,
            hashrate,
            cached_set_difficulty: None,
            cached_notify: None,
            pending_target: None,
            pending_hashrate: None,
            sv1_handshake_complete: AtomicBool::new(false),
            queued_sv1_handshake_messages: Vec::new(),
            processing_queued_sv1_handshake_responses: AtomicBool::new(false),
            pending_share: RefCell::new(None),
            upstream_target: None,
            last_job_received_time: None,
        }
    }

    pub fn set_pending_target(&mut self, new_target: Target) {
        self.pending_target = Some(new_target);
        debug!("Downstream {}: Set pending target", self.downstream_id);
    }

    pub fn set_pending_hashrate(&mut self, new_hashrate: Option<Hashrate>) {
        self.pending_hashrate = new_hashrate;
        debug!("Downstream {}: Set pending hashrate", self.downstream_id);
    }

    pub fn set_upstream_target(&mut self, upstream_target: Target) {
        self.upstream_target = Some(upstream_target);
        debug!(
            "Downstream {}: Set upstream target to {:?}",
            self.downstream_id, upstream_target
        );
    }
}
