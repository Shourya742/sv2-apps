use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockUpstream, WithSetup},
    start_jdc_with_identities,
    template_provider::DifficultyLevel,
    utils::get_available_address,
    *,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::{SetupConnectionSuccess, MESSAGE_TYPE_SETUP_CONNECTION},
    job_declaration_sv2::MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
    mining_sv2::MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
    parsers_sv2::{AnyMessage, CommonMessages, JobDeclaration, Mining},
};

// Verifies that when a per-upstream `user_identity` is set on a JDC upstream entry,
// that identity is sent as-is in `OpenExtendedMiningChannel` — no `.minerN` suffix appended.
#[tokio::test]
async fn jdc_sends_per_upstream_identity() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let mock_pool_addr = get_available_address();
    let mock_pool = MockUpstream::new(mock_pool_addr, WithSetup::no());
    let pool_sender = mock_pool.start().await;

    let (pool_sniffer, pool_sniffer_addr) =
        start_sniffer("pool", mock_pool_addr, false, vec![], None);

    const PER_UPSTREAM_IDENTITY: &str = "bc1qtest.worker";

    let (jdc, jdc_addr, _) = start_jdc_with_identities(
        &[(
            pool_sniffer_addr,
            jds_addr,
            PER_UPSTREAM_IDENTITY.to_string(),
        )],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    // Wait for JDC to attempt connection with the pool.
    pool_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    // Respond with success so JDC proceeds to connect to JDS and open channels.
    pool_sender
        .send(AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
            SetupConnectionSuccess {
                used_version: 2,
                flags: 0,
            },
        )))
        .await
        .unwrap();

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd, _) = start_minerd(tproxy_addr, None, None, false).await;

    pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    let oemc = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannel(msg)))) => break msg,
            _ => continue,
        }
    };

    let identity_str =
        std::str::from_utf8(oemc.user_identity.as_ref()).expect("user_identity is not valid UTF-8");
    assert_eq!(
        identity_str, PER_UPSTREAM_IDENTITY,
        "expected per-upstream identity '{PER_UPSTREAM_IDENTITY}', got '{identity_str}'"
    );

    shutdown_all!(translator, jdc);
}

// Verifies that when the primary pool disconnects, JDC falls back to the next upstream entry
// and sends *that* upstream's `user_identity` in `AllocateMiningJobToken` — not the primary's.
// All three JDC identity reads (OEMC, ExtendedChannel, AllocateMiningJobToken) share a single
// `read_identity()` source, so asserting one path is sufficient.
#[tokio::test]
async fn jdc_per_upstream_identity_switches_on_fallback() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (primary_pool, primary_pool_addr, primary_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (_fallback_pool, fallback_pool_addr, fallback_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    const PRIMARY_IDENTITY: &str = "bc1qprimary.worker";
    const FALLBACK_IDENTITY: &str = "bc1qfallback.worker";

    let (primary_jds_sniffer, primary_jds_sniffer_addr) =
        start_sniffer("primary-jds", primary_jds_addr, false, vec![], None);
    let (fallback_jds_sniffer, fallback_jds_sniffer_addr) =
        start_sniffer("fallback-jds", fallback_jds_addr, false, vec![], None);

    let (jdc, _jdc_addr, _) = start_jdc_with_identities(
        &[
            (
                primary_pool_addr,
                primary_jds_sniffer_addr,
                PRIMARY_IDENTITY.to_string(),
            ),
            (
                fallback_pool_addr,
                fallback_jds_sniffer_addr,
                FALLBACK_IDENTITY.to_string(),
            ),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    primary_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    primary_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;

    primary_pool.shutdown().await;

    fallback_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;

    let allocate_msg = loop {
        match fallback_jds_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::JobDeclaration(JobDeclaration::AllocateMiningJobToken(msg)))) => {
                break msg
            }
            _ => continue,
        }
    };
    let identity_str = std::str::from_utf8(allocate_msg.user_identifier.as_ref())
        .expect("user_identifier is not valid UTF-8");
    assert_eq!(
        identity_str, FALLBACK_IDENTITY,
        "expected fallback pool identity '{FALLBACK_IDENTITY}', got '{identity_str}'"
    );

    shutdown_all!(jdc);
}
