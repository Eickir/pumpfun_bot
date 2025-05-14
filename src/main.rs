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
use crate::modules::token_manager::token_manager::SellOrder;
use crate::modules::monitoring::transaction_verifier::market_cap;
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
use tracing::{error, info, warn};

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
use tokio::sync::oneshot;
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
    buy_mc: f64,
    confirmed: bool,        // false avant confirmation du buy, true après
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    info!("🚀 Bot Pump.fun lancé");

    // ───────────── infra de base
    let shutdown = CancellationToken::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    let tx_sem: Arc<Semaphore> = Arc::new(Semaphore::const_new(128));     // buy & sell

    // ───────────── wallet
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

    // ───────────── shared state
    let pending: PendingTxMap = Arc::new(DashMap::new());
    let created: Arc<DashMap<Pubkey, MintState>> = Arc::new(DashMap::new());

    // ───────────── gRPC
    let client = Arc::new(Client::new(env::var("GRPC_ENDPOINT")?, env::var("X_TOKEN")?)?);

    // >>> oneshot pour savoir quand le flux wallet est prêt
    let (ready_tx, mut ready_rx) = oneshot::channel();
    client.set_wallet_ready_notifier(ready_tx).await;

    // 1. Streams Yellowstone 
    let (blockmeta_tx, mut blockmeta_rx) = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx, mut pump_rx) = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx, wallet_rx) = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    tasks.push(tokio::spawn({
        let cli = Arc::clone(&client);
        async move {
            cli.subscribe_two_streams(Arc::new(blockmeta_tx), Arc::new(pump_tx), Arc::new(wallet_tx), 512).await;
        }
    }));

    // ██████████  2. Blockhash watch
    let (bh_tx, bh_rx) = watch::channel(Hash::default());
    let last_slot = Arc::new(AtomicU64::new(0));

    // ██████████  3. Rayon decode pool (Create+Trade / Trade)
    let (decode_req_tx, decode_req_rx): (FlumeSender<DecodeJob>, FlumeReceiver<DecodeJob>) = flume_bounded(4096);
    let (create_tx, mut create_rx) = mpsc::channel::<(CreateEvent, TradeEvent)>(512);
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
                            let _ = trade_tx.try_send(EnrichedTradeEvent {
                                trade: t.clone(),
                                tx_id: job.tx_id,
                                slot: job.slot,
                                tx_index: job.tx_index,
                            });
                            maybe_trade.get_or_insert(t);
                        }
                    }
                }
            }
            if let (Some(c), Some(t)) = (maybe_create, maybe_trade) {
                let _ = create_tx.try_send((c, t));
            }
        });
    });

    // ██████████  4. Manager & listener (nouveau sender sell)
    let (sell_req_tx, mut sell_req_rx) = mpsc::channel::<SellOrder>(256);
    let (manager_raw, mut event_rx) = TokenWorkerManager::new(1_000, sell_req_tx.clone());
    let manager = Arc::new(manager_raw);

    tasks.push(tokio::spawn({
        let shutdown_c = shutdown.child_token();
        async move {
            loop {
                tokio::select! {
                    Some(evt) = event_rx.recv() =>
                        info!("📈 Worker-trade: mint={} sol={:.4} slot={} sig={}",
                              evt.trade.mint,
                              evt.trade.sol_amount as f64 / 1e9,
                              evt.slot,
                              evt.tx_id),
                    _ = shutdown_c.cancelled() => break,
                }
            }
        }
    }));

    // ██████████  5. Watcher confirmations
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

    // ██████████  6. Boucle principale (buy & sell)
    tasks.push(tokio::spawn({
        let wallet_c  = Arc::clone(&wallet);
        let manager_c = Arc::clone(&manager);
        let tx_sem_c  = Arc::clone(&tx_sem);
        let bh_rx_c   = bh_rx.clone();
        let created_c = Arc::clone(&created);
        let pending_c = Arc::clone(&pending);
        let decode_req_tx_c = decode_req_tx.clone();
        let rpc_cfg_c = rpc_cfg.clone();
        let shutdown_c = shutdown.child_token();
        let mut wallet_ready = false;

        async move {
            loop {
                tokio::select! {
                    biased;

                    _ = &mut ready_rx, if !wallet_ready => {
                        wallet_ready = true;
                        info!("🔗 Analyse Pump.fun activée – wallet stream prêt");
                    }

                    /*───────────────────────────────────────────────
                     *  (a) Flux brut Pump.fun -> Rayon
                     *───────────────────────────────────────────────*/
                    Some(tx_arc) = pump_rx.recv() => {
                        if wallet_ready {
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
                    }

                    /*───────────────────────────────────────────────
                     *  (b) Tx Create+Trade ⇒ BUY
                     *───────────────────────────────────────────────*/
                    Some((create_evt, dev_trade)) = create_rx.recv() => {
                        let mint = create_evt.mint;
                        if created_c.contains_key(&mint) { continue; }
                        if !should_buy(dev_trade.sol_amount) { continue; }

                        /*─────────────────────── NEW ───────────────────────*/
                        // on mémorise bonding_curve & creator pour ce mint
                        manager_c.register_meta(&mint, &create_evt.bonding_curve, &create_evt.user);
                        /*────────────────────────────────────────────────────*/

                        created_c.insert(mint, MintState { create: create_evt.clone(), buy_mc: 0.0, confirmed: false });

                        // spawn l'achat
                        let wallet_b  = Arc::clone(&wallet_c);
                        let sem_b     = Arc::clone(&tx_sem_c);
                        let mut bh_rx_b = bh_rx_c.clone();
                        let pending_b = Arc::clone(&pending_c);
                        let created_b = Arc::clone(&created_c);
                        let manager_b = Arc::clone(&manager_c);
                        let rpc_cfg_b = rpc_cfg_c.clone();

                        tokio::spawn(async move {
                            let _p = sem_b.acquire().await;
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
                                    if wallet_b.rpc_client.send_transaction_with_config(&buy_tx, rpc_cfg_b).await.is_ok() {
                                        info!("➡️  Buy envoyé {mint} (sig={sig})");
                                    }
                                    while rx.changed().await.is_ok() {
                                        if *rx.borrow() == TxStatus::Successed {
                                            if let Some(conf) = pending_b.get(&sig) {
                                                let real_mc = market_cap(
                                                    conf.virtual_sol_reserves,
                                                    conf.virtual_token_reserves,
                                                );
                                                let qty = conf.token_amount;        // ← quantité reçue
                                                info!("✅ Buy confirmé {mint} – MC entrée ≈ {:.2} SOL", real_mc);
                                    
                                                if let Some(mut st) = created_b.get_mut(&mint) {
                                                    st.buy_mc   = real_mc;
                                                    st.confirmed = true;
                                                }

                                                /* NOUVEAU : communique le solde réel au manager */
                                                manager_b.update_balance(&mint, qty);
                                            }
                                            manager_b.ensure_worker(&mint.to_string());
                                            break;
                                        }
                                    }
                                }
                                Err(e) => error!("❌ buy {mint}: {e}"),
                            }
                        });
                    }

                    /*───────────────────────────────────────────────
                    *  (c) ORDRE DE VENTE – retry loop jusqu’à Successed
                    *───────────────────────────────────────────────*/
                    Some(order) = sell_req_rx.recv() => {
                        let wallet_s   = Arc::clone(&wallet_c);
                        let mut bh_rx_s = bh_rx_c.clone();
                        let sem_s      = Arc::clone(&tx_sem_c);
                        let pending_s  = Arc::clone(&pending_c);
                        let manager_s  = Arc::clone(&manager_c);
                        let rpc_cfg_s  = rpc_cfg_c.clone();

                        tokio::spawn(async move {
                            let _permit = sem_s.acquire().await;
                            let mut attempt: u8 = 0;

                            loop {
                                attempt += 1;
                                // rafraîchit le blockhash pour chaque essai
                                let bh = *bh_rx_s.borrow_and_update();

                                // 1. constr. transaction
                                let sell_tx = match wallet_s
                                    .sell_transaction(
                                        &order.mint,
                                        &order.bonding_curve,
                                        &order.creator,
                                        order.token_amount,
                                        bh
                                    ).await {
                                    Ok(tx) => tx,
                                    Err(e) => {
                                        error!("❌ build sell {} (try {}): {}", order.mint, attempt, e);
                                        tokio::time::sleep(Duration::from_millis(600)).await;
                                        continue;
                                    }
                                };

                                // 2. send
                                let sig = sell_tx.signatures[0];
                                let (tx_watch, mut rx) = watch::channel(TxStatus::Pending);
                                pending_s.insert(sig, ConfirmationState {
                                    token_pubkey: order.mint,
                                    bonding_curve: order.bonding_curve,
                                    token_amount: order.token_amount,
                                    sol_amount: 0,
                                    virtual_sol_reserves: 0,
                                    virtual_token_reserves: 0,
                                    notifier: tx_watch,
                                });

                                if let Err(e) = wallet_s
                                    .rpc_client
                                    .send_transaction_with_config(&sell_tx, rpc_cfg_s.clone())
                                    .await
                                {
                                    error!("❌ send sell {} (try {}): {}", order.mint, attempt, e);
                                    pending_s.remove(&sig);
                                    tokio::time::sleep(Duration::from_millis(600)).await;
                                    continue;                           // retry
                                }

                                info!("⬅️  Sell envoyé {} (sig={} try={})", order.mint, sig, attempt);

                                // 3. attend la confirmation avec timeout
                                let confirmed = tokio::time::timeout(
                                    Duration::from_secs(25),
                                    async {
                                        while rx.changed().await.is_ok() {
                                            match *rx.borrow() {
                                                TxStatus::Successed => return true,
                                                TxStatus::Failed    => return false,
                                                TxStatus::Pending   => continue,
                                            }
                                        }
                                        false
                                    }
                                ).await.unwrap_or(false);   // false si timeout

                                if confirmed {
                                    info!("💰 Sell confirmé {} ({} essais)", order.mint, attempt);
                                    manager_s.deduct_balance(&order.mint);
                                    break;
                                } else {
                                    warn!("⏳ Sell non confirmé {} (try {}), retry…", order.mint, attempt);
                                    pending_s.remove(&sig);          // on retire l’entrée ratée
                                    tokio::time::sleep(Duration::from_millis(800)).await;
                                }
                            } // loop
                        });
                    }


                    /*───────────────────────────────────────────────
                     *  (d) Trade générique ⇒ routage
                     *───────────────────────────────────────────────*/
                    Some(evt) = trade_rx.recv() => {
                        if let Some(state) = created_c.get(&evt.trade.mint) {
                            if state.confirmed {
                                let cur_mc = market_cap(
                                    evt.trade.virtual_sol_reserves,
                                    evt.trade.virtual_token_reserves
                                );
                                let pct = (cur_mc / state.buy_mc - 1.0) * 100.0;
                                info!("📊 {:?} MC: {:.2} → {:.2} SOL ({:+.1} %)",
                                      evt.trade.mint,
                                      state.buy_mc,
                                      cur_mc,
                                      pct);
                                manager_c.route_trade(&evt.trade.mint.to_string(), evt).await;
                            }
                        }
                    }

                    /*───────────────────────────────────────────────
                     *  (e) BlockMeta
                     *───────────────────────────────────────────────*/
                    Some(bm) = blockmeta_rx.recv() => {
                        if bm.slot > last_slot.load(Ordering::Relaxed) {
                            last_slot.store(bm.slot, Ordering::Relaxed);
                            if let Ok(h) = Hash::from_str(&bm.blockhash) {
                                let _ = bh_tx.send_replace(h);
                            }
                        }
                    }

                    /*───────────────────────────────────────────────
                     *  (f) shutdown
                     *───────────────────────────────────────────────*/
                    _ = shutdown_c.cancelled() => break,
                }
            }
        }
    }));

    // 7. Ctrl-C global
    signal::ctrl_c().await?;
    info!("🛑 SIGINT reçu – arrêt en cours…");
    shutdown.cancel();
    drop(decode_req_tx);
    sleep(Duration::from_secs(2)).await;
    for mut h in tasks {
        if h.is_finished() { let _ = h.await; }
        else { h.abort(); let _ = h.await; }
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
