//! Monitoring integration for Translation Proxy (tProxy)
//!
//! This module implements the ServerMonitoring trait on `ChannelManager`.
//! tProxy has server channels (upstream to pool) but no SV2 clients
//! (SV1 clients are handled separately in sv1_monitoring.rs).

use stratum_apps::monitoring::server::{ServerExtendedChannelInfo, ServerInfo, ServerMonitoring};

use crate::{sv2::channel_manager::ChannelManager, tproxy_mode, TproxyMode};

impl ServerMonitoring for ChannelManager {
    fn get_server(&self) -> ServerInfo {
        let mut extended_channels = Vec::new();
        let standard_channels = Vec::new(); // tProxy only uses extended channels

        match tproxy_mode() {
            TproxyMode::Aggregated => {
                self.upstream_extended_channel
                    .super_safe_lock(|upstream_channel| {
                        // In Aggregated mode: one shared upstream channel to the server
                        if let Some(upstream_channel) = upstream_channel.as_mut() {
                            let channel_id = upstream_channel.get_channel_id();
                            let target = upstream_channel.get_target();
                            let extranonce_prefix = upstream_channel.get_extranonce_prefix();
                            let user_identity = upstream_channel.get_user_identity();
                            let share_accounting = upstream_channel.get_share_accounting();

                            extended_channels.push(ServerExtendedChannelInfo {
                                channel_id,
                                user_identity: user_identity.clone(),
                                nominal_hashrate: upstream_channel.get_nominal_hashrate(),
                                target_hex: hex::encode(target.to_be_bytes()),
                                extranonce_prefix_hex: hex::encode(extranonce_prefix),
                                full_extranonce_size: upstream_channel.get_full_extranonce_size(),
                                rollable_extranonce_size: upstream_channel
                                    .get_rollable_extranonce_size(),
                                version_rolling: upstream_channel.is_version_rolling(),
                                shares_accepted: share_accounting.get_shares_accepted(),
                                share_work_sum: share_accounting.get_share_work_sum(),
                                last_share_sequence_number: share_accounting
                                    .get_last_share_sequence_number(),
                                best_diff: share_accounting.get_best_diff(),
                            });
                        }
                    });
            }
            TproxyMode::NonAggregated => {
                // In NonAggregated mode: each downstream Sv1 miner has its own upstream Sv2
                // channel to the server
                for channel in self.extended_channels.iter() {
                    let extended_channel = channel.value();

                    let channel_id = extended_channel.get_channel_id();
                    let target = extended_channel.get_target();
                    let extranonce_prefix = extended_channel.get_extranonce_prefix();
                    let user_identity = extended_channel.get_user_identity();
                    let share_accounting = extended_channel.get_share_accounting();

                    extended_channels.push(ServerExtendedChannelInfo {
                        channel_id,
                        user_identity: user_identity.clone(),
                        nominal_hashrate: extended_channel.get_nominal_hashrate(),
                        target_hex: hex::encode(target.to_be_bytes()),
                        extranonce_prefix_hex: hex::encode(extranonce_prefix),
                        full_extranonce_size: extended_channel.get_full_extranonce_size(),
                        rollable_extranonce_size: extended_channel.get_rollable_extranonce_size(),
                        version_rolling: extended_channel.is_version_rolling(),
                        shares_accepted: share_accounting.get_shares_accepted(),
                        share_work_sum: share_accounting.get_share_work_sum(),
                        last_share_sequence_number: share_accounting
                            .get_last_share_sequence_number(),
                        best_diff: share_accounting.get_best_diff(),
                    });
                }
            }
        }

        ServerInfo {
            extended_channels,
            standard_channels,
        }
    }
}
