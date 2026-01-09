//! Utilities for managing JDC communication, connection setup,
//! shutdown signaling, and upstream state tracking.
//!
//! This module provides:
//! - Construction of `SetupConnection` messages for mining, job declarator, and template
//!   distribution protocols.
//! - Helpers for parsing frames into typed Stratum messages.
//! - An async I/O task spawner for handling framed network communication with shutdown
//!   coordination.
//! - Deserialization of coinbase transaction outputs.
//! - Shutdown signaling types for orchestrating controlled shutdown of upstream, downstream, and
//!   job declarator components.
//! - An atomic wrapper for managing the upstream connection state safely across threads.
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use stratum_apps::{
    stratum_core::{
        binary_sv2::Str0255,
        channels_sv2::client,
        common_messages_sv2::{Protocol, SetupConnection},
        job_declaration_sv2::PushSolution,
        mining_sv2::{
            CloseChannel, OpenExtendedMiningChannel, OpenStandardMiningChannel,
            SubmitSharesExtended,
        },
        parsers_sv2::{JobDeclaration, Mining},
    },
    utils::types::{ChannelId, DownstreamId, Hashrate, JobId},
};
use tracing::{debug, info};

use crate::{
    channel_manager::{downstream_message_handler::RouteMessageTo, ChannelManagerData},
    config::ConfigJDCMode,
    error::JDCError,
};

/// Represents a message that can trigger shutdown of various system components.
#[derive(Debug, Clone)]
pub enum ShutdownMessage {
    /// Shutdown all components immediately
    ShutdownAll,
    /// Shutdown all downstream connections
    DownstreamShutdownAll,
    /// Shutdown a specific downstream connection by ID
    DownstreamShutdown(DownstreamId),
    /// Shutdown Upstream and JD part of JDC during fallback
    JobDeclaratorShutdownFallback((Vec<u8>, tokio::sync::mpsc::Sender<()>)),
    /// Shutdown Upstream and JD part during fallback
    UpstreamShutdownFallback((Vec<u8>, tokio::sync::mpsc::Sender<()>)),
    /// Shutdown Job Declarator during initialization.
    JobDeclaratorShutdown(tokio::sync::mpsc::Sender<()>),
    /// Shutdown Job Declarator during initialization.
    UpstreamShutdown(tokio::sync::mpsc::Sender<()>),
}

/// Constructs a `SetupConnection` message for the mining protocol.
pub fn get_setup_connection_message(
    min_version: u16,
    max_version: u16,
) -> Result<SetupConnection<'static>, JDCError> {
    let endpoint_host = "0.0.0.0".to_string().into_bytes().try_into()?;
    let vendor = String::new().try_into()?;
    let hardware_version = String::new().try_into()?;
    let firmware = String::new().try_into()?;
    let device_id = String::new().try_into()?;
    let flags = 0b0000_0000_0000_0000_0000_0000_0000_0110;
    Ok(SetupConnection {
        protocol: Protocol::MiningProtocol,
        min_version,
        max_version,
        flags,
        endpoint_host,
        endpoint_port: 50,
        vendor,
        hardware_version,
        firmware,
        device_id,
    })
}

/// Constructs a `SetupConnection` message for the Job Declarator (JDS).
pub fn get_setup_connection_message_jds(
    proxy_address: &SocketAddr,
    mode: &ConfigJDCMode,
) -> SetupConnection<'static> {
    let endpoint_host = proxy_address
        .ip()
        .to_string()
        .into_bytes()
        .try_into()
        .unwrap();
    let vendor = String::new().try_into().unwrap();
    let hardware_version = String::new().try_into().unwrap();
    let firmware = String::new().try_into().unwrap();
    let device_id = String::new().try_into().unwrap();
    let mut setup_connection = SetupConnection {
        protocol: Protocol::JobDeclarationProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
        endpoint_host,
        endpoint_port: proxy_address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    };

    if matches!(mode, ConfigJDCMode::FullTemplate) {
        setup_connection.allow_full_template_mode();
    }

    setup_connection
}

/// Constructs a `SetupConnection` message for the Template Provider (TP).
pub fn get_setup_connection_message_tp(address: SocketAddr) -> SetupConnection<'static> {
    let endpoint_host = address.ip().to_string().into_bytes().try_into().unwrap();
    let vendor = String::new().try_into().unwrap();
    let hardware_version = String::new().try_into().unwrap();
    let firmware = String::new().try_into().unwrap();
    let device_id = String::new().try_into().unwrap();
    SetupConnection {
        protocol: Protocol::TemplateDistributionProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
        endpoint_host,
        endpoint_port: address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    }
}

/// Represents the state of the upstream connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    /// No channel established with upstream.
    NoChannel = 0,
    /// Channel is being established undergoing.
    Pending = 1,
    /// Channel is active and connected.
    Connected = 2,
    /// Running in solo mining mode.
    SoloMining = 3,
}

/// Atomic wrapper for managing upstream connection state safely across threads.
#[derive(Clone)]
pub struct AtomicUpstreamState {
    inner: Arc<AtomicU8>,
}

impl AtomicUpstreamState {
    /// Creates a new atomic upstream state.
    pub fn new(state: UpstreamState) -> Self {
        Self {
            inner: Arc::new(AtomicU8::new(state as u8)),
        }
    }

    /// Returns the current upstream state.
    pub fn get(&self) -> UpstreamState {
        match self.inner.load(Ordering::SeqCst) {
            0 => UpstreamState::NoChannel,
            1 => UpstreamState::Pending,
            2 => UpstreamState::Connected,
            3 => UpstreamState::SoloMining,
            _ => unreachable!("invalid upstream state"),
        }
    }

    /// Updates the upstream state
    pub fn set(&self, state: UpstreamState) {
        self.inner.store(state as u8, Ordering::SeqCst);
    }

    /// Conditionally updates the upstream state if the current value matches.
    pub fn compare_and_set(
        &self,
        current: UpstreamState,
        new: UpstreamState,
    ) -> Result<(), UpstreamState> {
        self.inner
            .compare_exchange(current as u8, new as u8, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|v| match v {
                0 => UpstreamState::NoChannel,
                1 => UpstreamState::Pending,
                2 => UpstreamState::Connected,
                3 => UpstreamState::SoloMining,
                _ => unreachable!("invalid upstream state"),
            })
    }
}

/// Represents a pending channel request during the bootstrap phase
/// of the Job Declarator Client (JDC).  
///
/// These requests are created by downstreams that want to open
/// a mining channel but cannot proceed immediately.  
/// They remain queued until an upstream channel is successfully opened,
/// at which point they can be processed.
///
/// Two types of requests can be pending:
/// - [`OpenExtendedMiningChannel`] for extended mining channels
/// - [`OpenStandardMiningChannel`] for standard mining channels
pub enum PendingChannelRequest {
    /// A request to open an extended mining channel.
    ExtendedChannel {
        downstream_id: DownstreamId,
        message: OpenExtendedMiningChannel<'static>,
    },
    /// A request to open a standard mining channel.
    StandardChannel {
        downstream_id: DownstreamId,
        message: OpenStandardMiningChannel<'static>,
    },
}

impl From<(DownstreamId, OpenExtendedMiningChannel<'static>)> for PendingChannelRequest {
    fn from(value: (DownstreamId, OpenExtendedMiningChannel<'static>)) -> Self {
        PendingChannelRequest::ExtendedChannel {
            downstream_id: value.0,
            message: value.1,
        }
    }
}

impl From<(DownstreamId, OpenStandardMiningChannel<'static>)> for PendingChannelRequest {
    fn from(value: (DownstreamId, OpenStandardMiningChannel<'static>)) -> Self {
        PendingChannelRequest::StandardChannel {
            downstream_id: value.0,
            message: value.1,
        }
    }
}

impl PendingChannelRequest {
    pub fn downstream_id(&self) -> DownstreamId {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id,
                message: _,
            } => *downstream_id,
            PendingChannelRequest::StandardChannel {
                downstream_id,
                message: _,
            } => *downstream_id,
        }
    }

    pub fn message(self) -> Mining<'static> {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id: _,
                message: open_channel_message,
            } => Mining::OpenExtendedMiningChannel(open_channel_message),
            PendingChannelRequest::StandardChannel {
                downstream_id: _,
                message: open_channel_message,
            } => Mining::OpenStandardMiningChannel(open_channel_message),
        }
    }

    pub fn hashrate(&self) -> Hashrate {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id: _,
                message: m,
            } => m.nominal_hash_rate,
            PendingChannelRequest::StandardChannel {
                downstream_id: _,
                message: m,
            } => m.nominal_hash_rate,
        }
    }
}

/// Creates a [`CloseChannel`] message for the given channel ID and reason.
///
/// The `msg` is converted into a [`Str0255`] reason code.  
/// If conversion fails, this function will panic.
pub(crate) fn create_close_channel_msg(channel_id: ChannelId, msg: &str) -> CloseChannel<'_> {
    CloseChannel {
        channel_id,
        reason_code: Str0255::try_from(msg.to_string()).expect("Could not convert message."),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamChannelJobId {
    pub downstream_id: DownstreamId,
    pub channel_id: ChannelId,
    pub job_id: JobId,
}

impl From<(DownstreamId, ChannelId, JobId)> for DownstreamChannelJobId {
    fn from(value: (DownstreamId, ChannelId, JobId)) -> Self {
        DownstreamChannelJobId {
            downstream_id: value.0,
            channel_id: value.1,
            job_id: value.2,
        }
    }
}

pub fn validate_share(
    mut upstream_message: SubmitSharesExtended<'static>,
    channel_manager_data: &mut ChannelManagerData,
    messages: &mut Vec<RouteMessageTo>,
) {
    let Some(upstream_channel) = channel_manager_data.upstream_channel.as_mut() else {
        return;
    };
    let Some(prev_hash) = channel_manager_data.last_new_prev_hash.as_ref() else {
        return;
    };

    match upstream_channel.validate_share(upstream_message.clone()) {
        Ok(client::share_accounting::ShareValidationResult::Valid(share_hash)) => {
            upstream_message.sequence_number = channel_manager_data
                .sequence_number_factory
                .fetch_add(1, Ordering::Relaxed);

            info!(
                "Flushed cached extended share: valid | ch={}, seq={}, hash={} ✅",
                upstream_message.channel_id, upstream_message.sequence_number, share_hash
            );

            messages.push(Mining::SubmitSharesExtended(upstream_message.into_static()).into());
        }

        Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
            upstream_message.sequence_number = channel_manager_data
                .sequence_number_factory
                .fetch_add(1, Ordering::Relaxed);

            info!("💰 Block Found (cached extended)!!! 💰 {share_hash}");

            let mut channel_extranonce = upstream_channel.get_extranonce_prefix().to_vec();
            channel_extranonce.extend_from_slice(&upstream_message.extranonce.to_vec());

            let push_solution = PushSolution {
                extranonce: channel_extranonce.try_into().expect("extranonce"),
                ntime: upstream_message.ntime,
                nonce: upstream_message.nonce,
                version: upstream_message.version,
                nbits: prev_hash.n_bits,
                prev_hash: prev_hash.prev_hash.clone(),
            };

            messages.push(JobDeclaration::PushSolution(push_solution).into());
            messages.push(Mining::SubmitSharesExtended(upstream_message.into_static()).into());
        }

        Err(err) => {
            let code = match err {
                client::share_accounting::ShareValidationError::Invalid => "invalid-share",
                client::share_accounting::ShareValidationError::Stale => "stale-share",
                client::share_accounting::ShareValidationError::InvalidJobId => "invalid-job-id",
                client::share_accounting::ShareValidationError::DoesNotMeetTarget => {
                    "difficulty-too-low"
                }
                client::share_accounting::ShareValidationError::DuplicateShare => "duplicate-share",
                _ => unreachable!(),
            };

            debug!(
                "❌ Cached extended share rejected: ch={}, seq={}, error={}",
                upstream_message.channel_id, upstream_message.sequence_number, code
            );
        }
    }
}
