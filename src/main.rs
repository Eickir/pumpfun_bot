//! src/main.rs – v3.1 (fix build + petites retouches)

use dotenv::dotenv;
use std::{env, str::FromStr, sync::{Arc, atomic::{AtomicU64, Ordering}}};

use anyhow::Result;
use chrono::Local;
use dashmap::DashMap;
use flume::{bounded as flume_bounded, Receiver as FlumeReceiver, Sender as FlumeSender};
use futures_util::StreamExt;
use rayon::iter::ParallelBridge;
use rayon::prelude::*;
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
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info};

mod modules;
use crate::modules::{
    grpc_configuration::client::Client,
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

struct DecodeJob {
    slot: u64,
    tx_id: Signature,
    tx_index: u64,
    logs: Vec<Vec<u8>>,
}
struct DecodeResult {
    slot: u64,
    tx_id: Signature,
    tx_index: u64,
    create: Option<crate::modules::utils::types::CreateEvent>,
    dev_trade: Option<crate::modules::utils::types::TradeEvent>,
    follow: Vec<crate::modules::utils::types::TradeEvent>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    dotenv().ok();
    info!("🚀 Bot Pump.fun lancé");

    // RPC
    let rpc_cfg = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };

    // Wallet
    let kp = Keypair::from_bytes(&bs58::decode(env::var("SOLANA_PRIVATE_KEY")?).into_vec()?)?;
    let wallet = Arc::new(Wallet::new(kp, &env::var("RPC_ENDPOINT")?));
    let balance = wallet.get_balance().await.unwrap();
    info!("✅ Solde: {:.4} SOL", balance as f64 / 1e9);

    // Client gRPC
    let client = Arc::new(Client::new(
        env::var("GRPC_ENDPOINT")?,
        env::var("X_TOKEN")?,
    )?);

    // Canaux
    let (blockmeta_tx, mut blockmeta_rx) = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx,     mut pump_rx)      = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx,   wallet_rx)        = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (decode_res_tx, mut decode_res_rx) = mpsc::channel::<DecodeResult>(1024);

    // Streams Yellowstone
    {
        let cli = Arc::clone(&client);
        tokio::spawn(async move {
            cli.subscribe_two_streams(
                Arc::new(blockmeta_tx),
                Arc::new(pump_tx),
                Arc::new(wallet_tx),
                512,
            ).await;
        });
    }

    // Blockhash watch + slot
    let (bh_tx, bh_rx) = watch::channel(Hash::default());
    let last_slot = Arc::new(AtomicU64::new(0));

    // Rayon pool
    let (decode_req_tx, decode_req_rx): (FlumeSender<DecodeJob>, FlumeReceiver<DecodeJob>) =
        flume_bounded(4_096);
    {
        let tx_out = decode_res_tx.clone();
        rayon::spawn_fifo(move || {
            decode_req_rx.into_iter().par_bridge().for_each(|job| {
                let mut create = None;
                let mut dev    = None;
                let mut follow = Vec::with_capacity(job.logs.len());

                for raw in &job.logs {
                    if let Ok(evt) = decode_event(raw) {
                        match evt {
                            ParsedEvent::Create(e) => create = Some(e),
                            ParsedEvent::Trade(e)  => { dev = Some(e.clone()); follow.push(e); }
                        }
                    }
                }

                let _ = tx_out.try_send(DecodeResult {
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

    // Shared state
    let manager = Arc::new(TokenWorkerManager::new(1_000));
    let created = Arc::new(DashMap::<Pubkey, ()>::with_capacity(8_192));
    let buy_sem = Arc::new(Semaphore::const_new(128));

    // Boucle principale
    {
        let wallet  = Arc::clone(&wallet);
        let manager = Arc::clone(&manager);
        let created = Arc::clone(&created);
        let buy_sem = Arc::clone(&buy_sem);
        let last_slot_ref = Arc::clone(&last_slot);
        let bh_tx = bh_tx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Tx PumpFun (prioritaire)
                    Some(tx) = pump_rx.recv() => {
                        if let Some(info) = &tx.transaction {
                            let sig = Signature::try_from(info.signature.clone()).unwrap();
                            let _ = decode_req_tx.try_send(DecodeJob {
                                slot: tx.slot,
                                tx_id: sig,
                                tx_index: info.index,
                                logs: extract_program_logs(&tx),
                            });
                        }
                    }

                    // Décodage terminé
                    Some(res) = decode_res_rx.recv() => {
                        handle_decode_result(
                            res,
                            Arc::clone(&wallet),
                            rpc_cfg.clone(),
                            bh_rx.clone(),
                            Arc::clone(&created),
                            Arc::clone(&manager),
                            Arc::clone(&buy_sem),
                        ).await;
                    }

                    // BlockMeta
                    Some(bm) = blockmeta_rx.recv() => {
                        if bm.slot > last_slot_ref.load(Ordering::Relaxed) {
                            last_slot_ref.store(bm.slot, Ordering::Relaxed);
                            if let Ok(hash) = Hash::from_str(&bm.blockhash) {
                                let _ = bh_tx.send_replace(hash);
                            }
                        }
                    }

                    else => break,
                }
            }
        });
    }

    // wallet stream (debug)
    #[cfg(debug_assertions)]
    tokio::spawn(async move { ReceiverStream::new(wallet_rx).for_each(|_| async {}).await });
    #[cfg(not(debug_assertions))]
    drop(wallet_rx);

    signal::ctrl_c().await?;
    info!("🛑 Arrêt demandé – bye !");
    Ok(())
}

/// ------------------------------ Helpers ------------------------------------
#[inline(always)]
fn should_buy(sol: u64) -> bool {
    (500_000_000..=5_000_000_000).contains(&sol)
}

async fn handle_decode_result(
    res: DecodeResult,
    wallet: Arc<Wallet>,
    rpc_cfg: RpcSendTransactionConfig,
    bh_rx: watch::Receiver<Hash>,
    created: Arc<DashMap<Pubkey, ()>>,
    manager: Arc<TokenWorkerManager>,
    buy_sem: Arc<Semaphore>,
) {
    // 1) Achat
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
            let mut bh_rx = bh_rx.clone();
            let rpc_cfg_c = rpc_cfg.clone();

            tokio::spawn(async move {
                let _p = sem.acquire().await;
                let bh = *bh_rx.borrow_and_update();

                match wallet_c.buy_transaction(
                    &create_c.mint,
                    &create_c.bonding_curve,
                    &create_c.user,
                    0.001,
                    0.1,
                    trade_c,
                    bh,
                ).await {
                    Ok(buy_tx) => {
                        if wallet_c.rpc_client
                            .send_transaction_with_config(&buy_tx, rpc_cfg_c).await
                            .is_err()
                        {
                            error!("❌ Buy {} failed", mint);
                        } else {
                            info!("✅ Buy {} (sig={})", mint, buy_tx.signatures[0]);
                        }
                    }
                    Err(e) => error!("❌ Build tx {}: {e}", mint),
                }
            });
        }
    }

    // 2) Trades follow
    for tr in res.follow {
        let mint = tr.mint;
        if created.contains_key(&mint) {
            manager.route_trade(
                &mint.to_string(),
                EnrichedTradeEvent {
                    trade: tr,
                    tx_id: res.tx_id.clone(),
                    slot: res.slot,
                    tx_index: res.tx_index,
                },
            ).await;
        }
    }
}
