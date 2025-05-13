//! src/main.rs – v3.6  (shutdown propre, Rayon stoppé, gestion correcte des JoinHandle)

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
    time::{sleep, timeout, Duration},
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // ─────────────── Init & cancellation token ────────────────
    dotenv().ok();
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    info!("🚀 Bot Pump.fun lancé");

    let shutdown = CancellationToken::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // ─────────────── 1. Config & Wallet ───────────────────────
    let rpc_cfg = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };

    let keypair = Keypair::from_bytes(
        &bs58::decode(env::var("SOLANA_PRIVATE_KEY")?).into_vec()?,
    )?;
    let wallet = Arc::new(Wallet::new(keypair, &env::var("RPC_ENDPOINT")?));
    let balance = wallet.get_balance().await.unwrap();
    let wallet_balance = Arc::new(AtomicU64::new(balance));
    info!("✅ Solde: {:.4} SOL", balance as f64 / 1e9);

    // ─────────────── 2. Maps partagées ────────────────────────
    let pending: PendingTxMap = Arc::new(DashMap::new());
    let created: Arc<DashMap<Pubkey, ()>> =
        Arc::new(DashMap::with_capacity(8_192));

    // ─────────────── 3. gRPC Client ───────────────────────────
    let client = Arc::new(Client::new(
        env::var("GRPC_ENDPOINT")?,
        env::var("X_TOKEN")?,
    )?);

    // ─────────────── 4. Canaux Tokio ─────────────────────────
    let (blockmeta_tx, mut blockmeta_rx) =
        mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx, mut pump_rx) =
        mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx, wallet_rx) =
        mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);

    let (decode_res_tx, mut decode_res_rx) =
        mpsc::channel::<DecodeResult>(1024);

    // ─────────────── 5. Streams Yellowstone ──────────────────
    {
        let cli = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            cli.subscribe_two_streams(
                Arc::new(blockmeta_tx),
                Arc::new(pump_tx),
                Arc::new(wallet_tx),
                512,
            )
            .await;
        }));
    }

    // ─────────────── 6. Blockhash watch ──────────────────────
    let (bh_tx, bh_rx) = watch::channel(Hash::default());
    let last_slot = Arc::new(AtomicU64::new(0));

    // ─────────────── 7. Rayon decode pool ────────────────────
    let (decode_req_tx, decode_req_rx): (FlumeSender<DecodeJob>, FlumeReceiver<
        DecodeJob,
    >) =
        flume_bounded(4_096);
    {
        let out = decode_res_tx.clone();
        rayon::spawn_fifo(move || {
            decode_req_rx.into_iter().par_bridge().for_each(|job| {
                let mut create = None;
                let mut dev = None;
                let mut follow = Vec::with_capacity(job.logs.len());

                for raw in &job.logs {
                    if let Ok(evt) = decode_event(raw) {
                        match evt {
                            ParsedEvent::Create(e) => create = Some(e),
                            ParsedEvent::Trade(e) => {
                                dev = Some(e.clone());
                                follow.push(e);
                            }
                        }
                    }
                }

                let _ = out.try_send(DecodeResult {
                    slot: job.slot,
                    tx_id: job.tx_id,
                    tx_index: job.tx_index,
                    create,
                    dev_trade: dev,
                    follow,
                });
            });
        });
    }

    // ─────────────── 8. Autres partagés ───────────────────────
    let manager = Arc::new(TokenWorkerManager::new(1_000));
    let buy_sem = Arc::new(Semaphore::const_new(128));

    // ─────────────── 9. Vérificateur wallet ──────────────────
    {
        let pend = Arc::clone(&pending);
        let bal = Arc::clone(&wallet_balance);
        let mut w_rx = wallet_rx;
        let shutdown_c = shutdown.child_token();

        tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = confirm_wallet_transaction(w_rx, pend, bal) => (),
                _ = shutdown_c.cancelled() => info!("🔚 Arrêt du vérificateur wallet"),
            }
        }));
    }

    // ─────────────── 10. Boucle principale ───────────────────
    {
        let wallet_c = Arc::clone(&wallet);
        let manager_c = Arc::clone(&manager);
        let buy_sem_c = Arc::clone(&buy_sem);
        let bh_rx_c = bh_rx.clone();
        let pending_c = Arc::clone(&pending);
        let created_c = Arc::clone(&created);
        let decode_req_tx_c = decode_req_tx.clone();
        let shutdown_c = shutdown.child_token();

        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // 10-a) Ingest PumpFun
                    Some(tx_arc) = pump_rx.recv() => {
                        let tx = &*tx_arc;
                        if let Some(info) = &tx.transaction {
                            let sig = Signature::try_from(info.signature.clone()).unwrap();
                            let _ = decode_req_tx_c.try_send(DecodeJob {
                                slot: tx.slot,
                                tx_id: sig,
                                tx_index: info.index,
                                logs: extract_program_logs(tx),
                            });
                        }
                    }

                    // 10-b) DecodeResult
                    Some(res) = decode_res_rx.recv() => {
                        handle_decode_result(
                            res,
                            Arc::clone(&wallet_c),
                            rpc_cfg.clone(),
                            bh_rx_c.clone(),
                            Arc::clone(&created_c),
                            Arc::clone(&manager_c),
                            Arc::clone(&buy_sem_c),
                            Arc::clone(&pending_c),
                        ).await;
                    }

                    // 10-c) BlockMeta
                    Some(bm) = blockmeta_rx.recv() => {
                        if bm.slot > last_slot.load(Ordering::Relaxed) {
                            last_slot.store(bm.slot, Ordering::Relaxed);
                            if let Ok(h) = Hash::from_str(&bm.blockhash) {
                                let _ = bh_tx.send_replace(h);
                            }
                        }
                    }

                    // 10-d) Arrêt demandé
                    _ = shutdown_c.cancelled() => {
                        info!("🔚 Shutdown reçu dans la boucle principale");
                        break;
                    }
                }
            }
        }));
    }

    // ─────────────── 11. Gestion Ctrl-C ───────────────────────
    signal::ctrl_c().await?;
    info!("🛑 SIGINT reçu – arrêt en cours…");
    shutdown.cancel();          // réveille tous les select!
    drop(decode_req_tx);        // ferme le canal → Rayon sort

    // pause de grâce : 2 s
    sleep(Duration::from_secs(2)).await;

    // ─────────────── 12. Join / abort des tâches ─────────────
    for mut handle in tasks {
        if handle.is_finished() {
            let _ = handle.await;
        } else {
            handle.abort();
            let _ = handle.await;
        }
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
