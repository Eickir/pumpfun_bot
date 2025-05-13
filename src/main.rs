//! src/main.rs – v3.7  (shutdown propre + Rayon stoppé + journal des trades workers)

use dotenv::dotenv;
use std::{
    env,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::Result;
use dashmap::DashMap;
use flume::{bounded as flume_bounded, Receiver as FlumeReceiver, Sender as FlumeSender};
use rayon::{iter::ParallelBridge, prelude::*};
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::Signature,
    signer::keypair::Keypair,
};
use tokio::{
    signal,
    sync::{mpsc, watch, Semaphore},
    task::JoinHandle,
    time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

mod modules;
use crate::modules::{
    grpc_configuration::client::Client,
    monitoring::transaction_verifier::{
        confirm_wallet_transaction, ConfirmationState, PendingTxMap, TxStatus,
    },
    token_manager::token_manager::TokenWorkerManager,
    utils::{
        decoder::{decode_event, extract_program_logs},
        types::{EnrichedTradeEvent, ParsedEvent},
    },
    wallet::wallet::Wallet,
};
use yellowstone_grpc_proto::prelude::{
    SubscribeUpdateBlockMeta, SubscribeUpdateTransaction,
};
use crate::modules::utils::types::{CreateEvent, TradeEvent};

/// ------------------------------------------------------------------
/// Structures internes
/// ------------------------------------------------------------------
struct DecodeJob {
    slot:     u64,
    tx_id:    Signature,
    tx_index: u64,
    logs:     Vec<Vec<u8>>,
}
struct DecodeResult {
    slot:      u64,
    tx_id:     Signature,
    tx_index:  u64,
    create:    Option<crate::modules::utils::types::CreateEvent>,
    dev_trade: Option<crate::modules::utils::types::TradeEvent>,
    follow:    Vec<crate::modules::utils::types::TradeEvent>,
}

#[derive(Clone)]
struct MintState {
    create: CreateEvent,
    confirmed: bool,        // false avant confirmation du buy, true après
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // ───────────────────── init / runtime ─────────────────────
    dotenv().ok();
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    info!("🚀 Bot Pump.fun lancé");
    let shutdown = CancellationToken::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // ───────────────────── wallet / rpc cfg ───────────────────
    let rpc_cfg = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };
    let keypair = Keypair::from_bytes(&bs58::decode(env::var("SOLANA_PRIVATE_KEY")?).into_vec()?)?;
    let wallet = Arc::new(Wallet::new(keypair, &env::var("RPC_ENDPOINT")?));
    let balance = wallet.get_balance().await.unwrap();
    info!("✅ Solde: {:.4} SOL", balance as f64 / 1e9);

    let wallet_balance = Arc::new(AtomicU64::new(balance));

    // ───────────────────── shared maps ────────────────────────
    let pending: PendingTxMap          = Arc::new(DashMap::new());
    let created: Arc<DashMap<Pubkey, MintState>> = Arc::new(DashMap::new());

    // ───────────────────── gRPC client ────────────────────────
    let client = Arc::new(Client::new(env::var("GRPC_ENDPOINT")?, env::var("X_TOKEN")?)?);

    // ───────────────────── channels vers Yellowstone ──────────
    let (blockmeta_tx, mut blockmeta_rx) = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx, mut pump_rx)           = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx, wallet_rx)           = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    tasks.push(tokio::spawn({
        let cli = Arc::clone(&client);
        async move {
            cli.subscribe_two_streams(
                Arc::new(blockmeta_tx),
                Arc::new(pump_tx),
                Arc::new(wallet_tx),
                512,
            ).await;
        }
    }));

    // ───────────────────── blockhash watch ────────────────────
    let (bh_tx, bh_rx) = watch::channel(Hash::default());
    let last_slot = Arc::new(AtomicU64::new(0));

    // ───────────────────── Rayon decode pool ──────────────────
    let (decode_req_tx, decode_req_rx): (FlumeSender<DecodeJob>, FlumeReceiver<DecodeJob>) = flume_bounded(4096);
    let (create_tx, mut create_rx) = mpsc::channel::<(CreateEvent, TradeEvent, Signature, u64, u64)>(512);
    let (trade_tx,  mut trade_rx ) = mpsc::channel::<EnrichedTradeEvent>(4096);

    rayon::spawn_fifo(move || {
        decode_req_rx.into_iter().par_bridge().for_each(|job| {
            let mut maybe_create: Option<CreateEvent> = None;
            let mut maybe_trade : Option<TradeEvent>  = None;

            for raw in &job.logs {
                if let Ok(evt) = decode_event(raw) {
                    match evt {
                        ParsedEvent::Create(c) => maybe_create = Some(c),
                        ParsedEvent::Trade (t) => {
                            // chaque Trade est immédiatement publié
                            let _ = trade_tx.try_send(EnrichedTradeEvent {
                                trade: t.clone(),
                                tx_id: job.tx_id,
                                slot: job.slot,
                                tx_index: job.tx_index,
                            });
                            maybe_trade.get_or_insert(t);   // on garde le premier pour test dev
                        }
                    }
                }
            }
            // condition: "Create + Trade dans la même tx"
            if let (Some(c), Some(t)) = (maybe_create, maybe_trade) {
                let _ = create_tx.try_send((c, t, job.tx_id, job.slot, job.tx_index));
            }
        });
    });

    // ───────────────────── manager & listener ─────────────────
    let (manager_raw, mut event_rx) = TokenWorkerManager::new(1000);
    let manager = Arc::new(manager_raw);
    tasks.push(tokio::spawn({
        let shutdown_c = shutdown.child_token();
        async move {
            loop {
                tokio::select! {
                    Some(evt) = event_rx.recv() =>
                        info!("📈 Trade worker: mint={} sol={:.4} slot={} sig={}",
                              evt.trade.mint,
                              evt.trade.sol_amount as f64 / 1e9,
                              evt.slot,
                              evt.tx_id),
                    _ = shutdown_c.cancelled() => break,
                }
            }
        }
    }));

    // ───────────────────── confirmation watcher ───────────────
    tasks.push(tokio::spawn({
        let pend = Arc::clone(&pending);
        let bal  = Arc::clone(&wallet_balance);
        let mut w_rx = wallet_rx;
        let shutdown_c = shutdown.child_token();
        async move {
            tokio::select! {
                _ = confirm_wallet_transaction(w_rx, pend, bal) => (),
                _ = shutdown_c.cancelled() => (),
            }
        }
    }));

    // ───────────────────── divers objets partagés ─────────────
    let buy_sem: Arc<Semaphore> = Arc::new(Semaphore::const_new(128));

    // ───────────────────── boucle principale ──────────────────
    tasks.push(tokio::spawn({
        let wallet_c   = Arc::clone(&wallet);
        let manager_c  = Arc::clone(&manager);
        let buy_sem_c  = Arc::clone(&buy_sem);
        let bh_rx_c    = bh_rx.clone();
        let created_c  = Arc::clone(&created);
        let pending_c  = Arc::clone(&pending);
        let decode_req_tx_c = decode_req_tx.clone();
        let rpc_cfg_c  = rpc_cfg.clone();
        let shutdown_c = shutdown.child_token();

        async move {
            loop {
                tokio::select! {
                    biased;

                    // (1) brut Pump.fun -> Rayon
                    Some(tx_arc) = pump_rx.recv() => {
                        if let Some(info) = &tx_arc.transaction {
                            let sig = Signature::try_from(info.signature.clone()).unwrap();
                            let _ = decode_req_tx_c.try_send(DecodeJob {
                                slot: tx_arc.slot,
                                tx_id: sig,
                                tx_index: info.index,
                                logs: extract_program_logs(&tx_arc),
                            });
                        }
                    }

                    // (2) event Create+Trade => buy
                    Some((create_evt, dev_trade, tx_sig, slot, idx)) = create_rx.recv() => {
                        let mint = create_evt.mint;
                        if created_c.contains_key(&mint) {
                            continue; // déjà traité
                        }
                        // should_buy sur le trade DEV
                        if !should_buy(dev_trade.sol_amount) {
                            continue;
                        }
                        // on insère l'état avec confirmed=false
                        created_c.insert(mint, MintState { create: create_evt.clone(), confirmed: false });

                        // lancer l'achat dans une tâche séparée
                        let wallet_b   = Arc::clone(&wallet_c);
                        let sem_b      = Arc::clone(&buy_sem_c);
                        let mut bh_rx_b = bh_rx_c.clone();
                        let pending_b  = Arc::clone(&pending_c);
                        let created_b  = Arc::clone(&created_c);
                        let manager_b  = Arc::clone(&manager_c);
                        let rpc_cfg_b  = rpc_cfg_c.clone();

                        tokio::spawn(async move {
                            let _permit = sem_b.acquire().await;
                            let bh = *bh_rx_b.borrow_and_update();
                            match wallet_b.buy_transaction(
                                    &mint,
                                    &create_evt.bonding_curve,
                                    &create_evt.user,
                                    0.001,
                                    0.1,
                                    dev_trade.clone(),
                                    bh
                                ).await {
                                Ok(buy_tx) => {
                                    let sig = buy_tx.signatures[0];
                                    // notifier pour le watcher de confirmation
                                    let (tx_watch, mut rx) = watch::channel(TxStatus::Pending);
                                    pending_b.insert(sig, ConfirmationState {
                                        token_pubkey: mint,
                                        bonding_curve: create_evt.bonding_curve,
                                        token_amount: 0,
                                        sol_amount: 0,
                                        virtual_sol_reserves: 0,
                                        virtual_token_reserves: 0,
                                        notifier: tx_watch,
                                    });
                                    // envoi
                                    if wallet_b.rpc_client.send_transaction_with_config(&buy_tx, rpc_cfg_b).await.is_ok() {
                                        info!("➡️  Buy envoyé {mint} (sig={sig})");
                                    }
                                    // attendre confirmation
                                    while rx.changed().await.is_ok() {
                                        if *rx.borrow() == TxStatus::Successed {
                                            info!("✅ Buy confirmé pour {mint}");
                                            // maj drapeau + lancement worker
                                            created_b.insert(mint, MintState { create: create_evt.clone(), confirmed: true });
                                            manager_b.ensure_worker(&mint.to_string());
                                            break;
                                        }
                                    }
                                }
                                Err(e) => error!("❌ Construction/envoi buy {mint}: {e}"),
                            }
                        });
                    }

                    // (3) Trade générique -> route si buy confirmé
                    Some(evt) = trade_rx.recv() => {
                        if let Some(state) = created_c.get(&evt.trade.mint) {
                            if state.confirmed {
                                manager_c.route_trade(&evt.trade.mint.to_string(), evt).await;
                            }
                        }
                    }

                    // (4) BlockMeta -> refresh blockhash
                    Some(bm) = blockmeta_rx.recv() => {
                        if bm.slot > last_slot.load(Ordering::Relaxed) {
                            last_slot.store(bm.slot, Ordering::Relaxed);
                            if let Ok(h) = Hash::from_str(&bm.blockhash) {
                                let _ = bh_tx.send_replace(h);
                            }
                        }
                    }

                    // (5) Ctrl-C
                    _ = shutdown_c.cancelled() => break,
                }
            }
        }
    }));

    // ───────────────────── Ctrl-C global ─────────────────────
    signal::ctrl_c().await?;
    info!("🛑 SIGINT reçu – arrêt en cours…");
    shutdown.cancel();
    drop(decode_req_tx);
    sleep(Duration::from_secs(2)).await;

    for mut h in tasks {
        if h.is_finished() { let _ = h.await; } else { h.abort(); let _ = h.await; }
    }
    info!("👋 Bye !");
    Ok(())
}

/// ------------------------------------------------------------------
/// Helpers
/// ------------------------------------------------------------------
#[inline(always)]
fn should_buy(sol: u64) -> bool {
    (500_000_000..=5_000_000_000).contains(&sol)
}

async fn handle_decode_result(
    res: DecodeResult,
    wallet: Arc<Wallet>,
    rpc_cfg: RpcSendTransactionConfig,
    mut bh_rx: watch::Receiver<Hash>,
    created: Arc<DashMap<Pubkey, ()>>,
    manager: Arc<TokenWorkerManager>,
    buy_sem: Arc<Semaphore>,
    pending: PendingTxMap,
) {
    // (1) tentative d’achat
    if let (Some(create), Some(trade)) = (&res.create, &res.dev_trade) {
        if should_buy(trade.sol_amount) {
            let mint = create.mint;

            if created.insert(mint, ()).is_some() {
                manager.ensure_worker(&mint.to_string());
            }

            let wallet_c = Arc::clone(&wallet);
            let sem = Arc::clone(&buy_sem);
            let trade_c = trade.clone();
            let create_c = create.clone();
            let rpc_cfg_c = rpc_cfg.clone();
            let pending_c = Arc::clone(&pending);

            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let bh = *bh_rx.borrow_and_update();

                match wallet_c
                    .buy_transaction(
                        &create_c.mint,
                        &create_c.bonding_curve,
                        &create_c.user,
                        0.001,
                        0.1,
                        trade_c.clone(),
                        bh,
                    )
                    .await
                {
                    Ok(buy_tx) => {
                        let sig = buy_tx.signatures[0];
                        let (tx_watch, _rx) = watch::channel(TxStatus::Pending);
                        pending_c.insert(sig, ConfirmationState {
                            token_pubkey: create_c.mint,
                            bonding_curve: create_c.bonding_curve,
                            token_amount: 0,
                            sol_amount: 0,
                            virtual_sol_reserves: 0,
                            virtual_token_reserves: 0,
                            notifier: tx_watch,
                        });

                        match wallet_c
                            .rpc_client
                            .send_transaction_with_config(&buy_tx, rpc_cfg_c)
                            .await
                        {
                            Ok(_)  => info!("➡️  Buy envoyé {mint} (sig={sig})"),
                            Err(e) => error!("❌ Envoi buy {mint}: {e}"),
                        }
                    }
                    Err(e) => error!("❌ Construction buy {mint}: {e}"),
                }
            });
        }
    }

    // (2) trades suivants
    for tr in res.follow {
        if created.contains_key(&tr.mint) {
            manager
                .route_trade(
                    &tr.mint.to_string(),
                    EnrichedTradeEvent {
                        trade: tr.clone(),
                        tx_id: res.tx_id.clone(),
                        slot: res.slot,
                        tx_index: res.tx_index,
                    },
                )
                .await;
        }
    }
}
