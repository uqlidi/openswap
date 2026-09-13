//! Taker recovery discards a Legacy swapcoin whose contract can never be signed.
//!
//! Funding setup persists the outgoing swapcoins before the first maker is
//! asked for its contract signature, and a pre-funding abort only drops the
//! tracker record. After a restart the taker finds that swapcoin and starts its
//! recovery loop. The contract tx cannot be signed, so it never reaches the
//! chain, and the loop used to retry it forever: `is_recovery_complete()` never
//! turned true.
//!
//! Scenario:
//! 1. Both makers close at `ReqContractSigsForSender`, so the swap fails before
//!    anything is signed or broadcast, whichever maker is the first hop.
//! 2. The taker is restarted from the same data dir.
//! 3. The wallet coins are swept to an outside address and the sweep confirms,
//!    so the funding tx conflicts with a confirmed spend and can never confirm.
//! 4. Recovery must discard the swapcoin and finish.

use bitcoin::{
    secp256k1::{rand::rngs::OsRng, Secp256k1, SecretKey},
    Address, Amount, Network,
};
use bitcoind::bitcoincore_rpc::RpcApi;
use openswap::{
    maker::{start_server, MakerBehavior},
    protocol::common_messages::ProtocolVersion,
    taker::{SwapParams, Taker, TakerBehavior},
    utill::{MIN_FEE_RATE, NO_SHUTDOWN},
    wallet::{AddressType, Destination},
};

use super::test_framework::*;

use log::{info, warn};
use std::{
    sync::atomic::Ordering::Relaxed,
    thread,
    time::{Duration, Instant},
};

#[test]
fn test_legacy_unsignable_contract_discarded_after_funding_conflict() {
    warn!("Running Test: Legacy Unsignable Contract Recovery");

    let makers_config_map = vec![(9002, Some(21901)), (19002, Some(21902))];
    let taker_behavior = vec![TakerBehavior::Normal];
    let maker_behaviors = vec![
        MakerBehavior::CloseAtReqContractSigsForSender,
        MakerBehavior::CloseAtReqContractSigsForSender,
    ];

    let (test_framework, mut takers, makers, block_generation_handle) =
        TestFramework::init::<BitcoindBackend>(makers_config_map, taker_behavior, maker_behaviors);

    let bitcoind = &test_framework.bitcoind;
    // Owned, not borrowed: this taker gets dropped mid-test.
    let mut taker = takers.remove(0);

    fund_taker(
        &taker,
        bitcoind,
        3,
        Amount::from_btc(0.05).unwrap(),
        AddressType::P2TR,
    );
    fund_makers(
        &makers,
        bitcoind,
        4,
        Amount::from_btc(0.05).unwrap(),
        AddressType::P2TR,
    );

    info!("Starting Maker servers...");
    let maker_threads = makers
        .iter()
        .map(|maker| {
            let maker_clone = maker.clone();
            thread::spawn(move || {
                start_server(maker_clone).unwrap();
            })
        })
        .collect::<Vec<_>>();

    wait_for_makers_setup(&makers, 120);

    for maker in &makers {
        maker
            .wallet
            .write()
            .unwrap()
            .sync_and_save(&NO_SHUTDOWN)
            .unwrap();
    }

    let swap_params = SwapParams::new(ProtocolVersion::Legacy, Amount::from_sat(500000), 2)
        .with_tx_count(3)
        .with_required_confirms(1);

    generate_blocks(bitcoind, 1);

    let summary = taker
        .prepare_swap(swap_params)
        .expect("Prepare should succeed");
    let swap_result = taker.start_swap(&summary.swap_id);
    assert!(
        swap_result.is_err(),
        "Swap should fail at the sender contract signature request"
    );
    info!("Swap failed as expected: {:?}", swap_result.err().unwrap());

    let persisted = taker
        .get_wallet()
        .read()
        .unwrap()
        .get_outgoing_swapcoins_count();
    assert!(
        persisted > 0,
        "the failed swap should leave its unsigned outgoing swapcoins in the wallet"
    );

    makers
        .iter()
        .for_each(|maker| maker.shutdown.store(true, Relaxed));
    maker_threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());

    info!("Restarting the taker");
    drop(taker);
    thread::sleep(Duration::from_secs(5));

    let restarted = Taker::init(test_framework.taker_init_config::<BitcoindBackend>(0))
        .expect("restarted taker should open the same wallet");

    // Sweep every wallet coin elsewhere. The funding tx spends some of them, so
    // once the sweep confirms the funding tx can never confirm.
    {
        let mut wallet = restarted.get_wallet().write().unwrap();
        wallet.sync_and_save(&NO_SHUTDOWN).unwrap();
        let coins = wallet.list_descriptor_utxo_spend_info();
        assert!(!coins.is_empty(), "taker should still hold its coins");

        let secp = Secp256k1::new();
        let keypair = bitcoin::key::Keypair::from_secret_key(&secp, &SecretKey::new(&mut OsRng));
        let (xonly, _) = keypair.x_only_public_key();
        let addr = Address::p2tr(&secp, xonly, None, Network::Regtest);

        let sweep = wallet
            .spend_from_wallet(MIN_FEE_RATE, Destination::Sweep(addr), &coins)
            .unwrap();
        bitcoind.client.send_raw_transaction(&sweep).unwrap();
    }
    generate_blocks(bitcoind, 1);

    info!("Waiting for the restarted taker's recovery loop to finish...");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !restarted.is_recovery_complete() {
        assert!(
            Instant::now() < deadline,
            "recovery never finished: the unsignable swapcoin was not discarded"
        );
        thread::sleep(Duration::from_secs(5));
    }

    assert_eq!(
        restarted
            .get_wallet()
            .read()
            .unwrap()
            .get_outgoing_swapcoins_count(),
        0,
        "restarted taker still holds outgoing swapcoins; before={persisted}"
    );

    info!("Legacy unsignable contract recovery test completed successfully!");

    test_framework.stop();
    block_generation_handle.join().unwrap();
}
