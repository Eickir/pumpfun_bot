use dotenv::dotenv;
use std::{env, sync::Arc, str::FromStr};
use anyhow::Result;
use tracing::{info, error};
use tokio::{signal, task};
use tokio::sync::{mpsc, oneshot};
use solana_sdk::{hash::Hash, signature::Signature};
use solana_sdk::signer::keypair::Keypair;
use dashmap::DashMap;
use chrono::Local;
use arc_swap::ArcSwap;
use tokio_stream::wrappers::ReceiverStream;
use crossbeam::channel::unbounded;
use yellowstone_grpc_proto::prelude::{SubscribeUpdateBlockMeta, SubscribeUpdateTransaction};
use solana_client::rpc_config::RpcSendTransactionConfig;

mod modules;
use crate::modules::{
    grpc_configuration::client::Client,
    token_manager::token_manager::TokenWorkerManager,
    utils::decoder::{extract_program_logs, decode_event},
    utils::types::{ParsedEvent, EnrichedTradeEvent},
    utils::blockmeta_handler::{BlockMetaEntry, SharedBlockMeta},
    wallet::wallet::Wallet,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // 1) Logger au niveau INFO
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    dotenv().ok();

    info!("🚀 Démarrage du bot gRPC…");

    // 2) Config RPC Solana
    let rpc_config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };

    // 3) Wallet setup
    let sk       = env::var("SOLANA_PRIVATE_KEY")?;
    let bytes    = bs58::decode(sk).into_vec()?;
    let keypair  = Keypair::from_bytes(&bytes)?;
    let rpc_url  = env::var("RPC_ENDPOINT")?;
    let wallet   = Arc::new(Wallet::new(keypair, &rpc_url));
    let balance  = wallet.get_balance().await.unwrap();
    info!("✅ Wallet connecté, SOL disponible : {:.6} SOL", balance as f64 / 1e9);

    // 4) gRPC client
    let endpoint = env::var("GRPC_ENDPOINT")?;
    let x_token  = env::var("X_TOKEN")?;
    let client   = Arc::new(Client::new(endpoint, x_token)?);

    // 5) Channels Tokio MPSC pour BlockMeta + Pump + Wallet tx
    let (blockmeta_tx, mut blockmeta_rx) = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx,      mut pump_rx)      = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx,    wallet_rx)        = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);

    // 6) Lancement des streams gRPC
    {
        let client    = Arc::clone(&client);
        let bm_chan   = Arc::new(blockmeta_tx);
        let pump_chan = Arc::new(pump_tx);
        let wal_chan  = Arc::new(wallet_tx);
        info!("🔗 Lancement des streams gRPC");
        tokio::spawn(async move {
            client
                .subscribe_two_streams(bm_chan, pump_chan, wal_chan, 2000)
                .await;
        });
    }

    // 7) Shared blockmeta
    let initial_meta = BlockMetaEntry {
        slot:        0,
        detected_at: Local::now(),
        blockhash:   Hash::default(),
    };
    let shared_blockmeta: SharedBlockMeta =
        Arc::new(ArcSwap::new(Arc::new(initial_meta)));

    // 8) Worker dédié de décodage (crossbeam unbounded)
    //
    //    On reçoit (logs, reply_chan), on décode tout en synchrone sur ce thread,
    //    puis on renvoie Vec<ParsedEvent> via oneshot::Sender.
    let (decode_tx, decode_rx) =
        unbounded::<(Vec<Vec<u8>>, oneshot::Sender<Vec<ParsedEvent>>)>();
    std::thread::spawn(move || {
        for (logs, reply) in decode_rx.iter() {
            let mut events = Vec::with_capacity(logs.len());
            for raw in logs {
                if let Ok(evt) = decode_event(&raw) {
                    events.push(evt);
                }
            }
            let _ = reply.send(events);
        }
    });

    // 9) Pipeline principal : select! entre blockmeta et pump
    {
        let shared   = Arc::clone(&shared_blockmeta);
        let manager  = Arc::new(TokenWorkerManager::new(1_000));
        let created  = Arc::new(DashMap::<String, ()>::new());
        let wallet   = Arc::clone(&wallet);
        let rpc_conf = rpc_config.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // ─── BlockMeta ─────────────────────────────
                    Some(bm_arc) = blockmeta_rx.recv() => {
                        if let Ok(h) = Hash::from_str(&bm_arc.blockhash) {
                            shared.store(Arc::new(BlockMetaEntry {
                                slot:        bm_arc.slot,
                                detected_at: Local::now(),
                                blockhash:   h,
                            }));
                        }
                    }

                    // ─── Transaction PumpFun ──────────────────
                    Some(tx_arc) = pump_rx.recv() => {
                        let tx = &*tx_arc;
                        // on garde le dernier blockhash gRPC
                        let blockhash = shared.load().blockhash;

                        // signature + index
                        let (tx_id, tx_index) =
                            if let Some(info) = tx.transaction.as_ref() {
                                let sig = Signature::try_from(info.signature.clone()).unwrap();
                                (sig, info.index)
                            } else { continue; };

                        // 1) extraction des logs
                        let logs = extract_program_logs(tx);

                        // 2) envoi au worker de décodage + await réponse
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let _ = decode_tx.send((logs, resp_tx));
                        let parsed = resp_rx.await.unwrap_or_default();

                        // 3) séparation des événements
                        let mut create_evt = None;
                        let mut dev_trade  = None;
                        let mut follow     = Vec::new();
                        for evt in parsed {
                            match evt {
                                ParsedEvent::Create(e) => create_evt = Some(e),
                                ParsedEvent::Trade(e)  => {
                                    dev_trade = Some(e.clone());
                                    follow.push(e);
                                }
                            }
                        }

                        // 4) buy_tx si on a les 2 événements
                        if let (Some(create), Some(trade)) = (create_evt, dev_trade) {
                            if trade.sol_amount <= 2_000_000_000 {
                                let token_id = create.mint.to_string();
                                let buy_tx = wallet.buy_transaction(
                                        &create.mint,
                                        &create.bonding_curve,
                                        0.001,
                                        0.1,
                                        trade.clone(),
                                        blockhash,
                                    )
                                    .await
                                    .unwrap();

                                // zero-alloc : on loggue directement la signature
                                let sig = buy_tx.signatures[0];
                                let wallet2   = Arc::clone(&wallet);
                                let rpc_conf2 = rpc_conf.clone();
                                let tok2      = token_id.clone();
                                task::spawn(async move {
                                    match wallet2
                                        .rpc_client
                                        .send_transaction_with_config(&buy_tx, rpc_conf2)
                                        .await
                                    {
                                        Ok(_) => info!("✅ Buy succeeded for {} (sig={})", tok2, sig),
                                        Err(e) => error!("❌ Buy failed for {} (sig={}): {:?}", tok2, sig, e),
                                    }
                                });

                                // démarrage du worker métier
                                if created.insert(token_id.clone(), ()).is_none() {
                                    manager.ensure_worker(&token_id);
                                }
                            }
                        }

                        // 5) routage des trades suivants
                        for e in follow {
                            let tid = e.mint.to_string();
                            if created.contains_key(&tid) {
                                manager.route_trade(
                                    &tid,
                                    EnrichedTradeEvent {
                                        trade:    e.clone(),
                                        tx_id:    tx_id.clone(),
                                        slot:     tx.slot,
                                        tx_index,
                                    },
                                )
                                .await;
                            }
                        }
                    }

                    // ─── Plus rien à lire → fin propre ────────
                    else => break,
                }
            }
        });
    }

    // 10) On peut toujours logger les tx du wallet si besoin, ou laisser muet
    let mut _ws = ReceiverStream::new(wallet_rx);
    // …

    // 11) Attente Ctrl-C puis sortie
    signal::ctrl_c().await?;
    info!("🛑 Fin du bot");
    Ok(())
}
