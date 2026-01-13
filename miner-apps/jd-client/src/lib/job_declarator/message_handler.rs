use stratum_apps::stratum_core::{
    common_messages_sv2::{
        ChannelEndpointChanged, Reconnect, SetupConnectionError, SetupConnectionSuccess,
    },
    handlers_sv2::HandleCommonMessagesFromServerAsync,
    parsers_sv2::Tlv,
};
use tracing::{info, warn};

use crate::{
    config::ConfigJDCMode,
    error::JDCError,
    jd_mode::{set_jd_mode, JdMode},
    job_declarator::JobDeclarator,
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromServerAsync for JobDeclarator {
    type Error = JDCError;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(vec![])
    }

    async fn handle_setup_connection_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionSuccess,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Setting up JDMode from config, upon
        // successful handshake.
        let jd_mode = match self.mode {
            ConfigJDCMode::CoinbaseOnly => JdMode::CoinbaseOnly,
            ConfigJDCMode::FullTemplate => JdMode::FullTemplate,
        };

        set_jd_mode(jd_mode);

        Ok(())
    }

    async fn handle_channel_endpoint_changed(
        &mut self,
        _server_id: Option<usize>,
        msg: ChannelEndpointChanged,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        Ok(())
    }

    async fn handle_reconnect(
        &mut self,
        _server_id: Option<usize>,
        msg: Reconnect<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        Ok(())
    }

    async fn handle_setup_connection_error(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        Err(JDCError::Shutdown)
    }
}
