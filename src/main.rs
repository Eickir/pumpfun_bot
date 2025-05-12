use dotenv::dotenv;
use std::{env, sync::Arc, str::FromStr};
use anyhow::Result;
use tracing::{info, error};
use tokio::{signal, sync::mpsc, task};
use futures_util::{stream::StreamExt};
use tokio_stream::StreamMap;
use futures_util::future::Either;
use solana_sdk::{hash::Hash, signature::Signature};
use tokio_stream::wrappers::ReceiverStream;
use solana_sdk::signer::keypair::Keypair;
use dashmap::DashMap;
use chrono::Local;
use arc_swap::ArcSwap;
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
    // 1) Logger + .env
    tracing_subscriber::fmt().init();
    dotenv().ok();

    // 2) RPC Solana config
    let rpc_config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };
    info!("🚀 Démarrage du bot gRPC…");

    // 3) Wallet setup
    let sk = env::var("SOLANA_PRIVATE_KEY")?;
    let bytes = bs58::decode(sk).into_vec()?;
    let keypair = Keypair::from_bytes(&bytes)?;
    let rpc_url = env::var("RPC_ENDPOINT")?;
    let wallet = Arc::new(Wallet::new(keypair, &rpc_url));
    let balance = wallet.get_balance().await.unwrap();
    info!("Wallet connecté, SOL disponible : {}", balance as f64 / 1e9);

    // 4) gRPC client
    let endpoint = env::var("GRPC_ENDPOINT")?;
    let x_token  = env::var("X_TOKEN")?;
    let client   = Arc::new(Client::new(endpoint, x_token)?);

    // 5) Canaux Tokio MPSC (zero-copy via Arc)
    let (blockmeta_tx, blockmeta_rx) = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx,      pump_rx)      = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx,    wallet_rx)    = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);

    // 6) Lancement des flux gRPC
    {
        let client    = Arc::clone(&client);
        let bm_chan   = Arc::new(blockmeta_tx);
        let pump_chan = Arc::new(pump_tx);
        let wal_chan  = Arc::new(wallet_tx);
        tokio::spawn(async move {
            client
                .subscribe_two_streams(bm_chan, pump_chan, wal_chan, 2000)
                .await;
        });
    }

    // 7) Stockage atomique du dernier BlockMeta pour fallback
    let initial_meta = BlockMetaEntry {
        slot: 0,
        detected_at: Local::now(),
        blockhash: Hash::default(),
    };
    let shared_blockmeta: SharedBlockMeta =
        Arc::new(ArcSwap::new(Arc::new(initial_meta)));

    // 8) Fusion des flux BlockMeta + PumpFun
    let mut sm = StreamMap::new();
    sm.insert(
        "bm",
        ReceiverStream::new(blockmeta_rx)
            .map(Either::Left)
            .boxed(),
    );
    sm.insert(
        "pu",
        ReceiverStream::new(pump_rx)
            .map(Either::Right)
            .boxed(),
    );

    // 9) Traitement unifié
    {
        let shared    = Arc::clone(&shared_blockmeta);
        let manager   = Arc::new(TokenWorkerManager::new(1_000));
        let created   = Arc::new(DashMap::<String, ()>::new());
        let wallet    = Arc::clone(&wallet);
        let rpc_conf  = rpc_config.clone();

        tokio::spawn(async move {
            sm.for_each_concurrent(Some(1000), move |(_key, item)| {
                let shared    = Arc::clone(&shared);
                let manager   = Arc::clone(&manager);
                let created   = Arc::clone(&created);
                let wallet    = Arc::clone(&wallet);
                let rpc_conf  = rpc_conf.clone();

                async move {
                    match item {
                        // ** Mise à jour du dernier BlockMeta reçu **
                        Either::Left(bm_arc) => {
                            if let Ok(h) = Hash::from_str(&bm_arc.blockhash) {
                                let entry = BlockMetaEntry {
                                    slot:        bm_arc.slot,
                                    detected_at: Local::now(),
                                    blockhash:   h,
                                };
                                shared.store(Arc::new(entry));
                            }
                        }

                        // ** Traitement des transactions PumpFun **
                        Either::Right(tx_arc) => {
                            let tx = &*tx_arc;

                            // On récupère toujours le blockhash issu du dernier BlockMeta
                            let blockhash = shared.load().blockhash;

                            // Signature + index
                            let (tx_id, tx_index) = if let Some(info) = tx.transaction.as_ref() {
                                let sig = Signature::try_from(info.signature.clone()).unwrap();
                                (sig, info.index)
                            } else {
                                return;
                            };

                            // ** Décodage CPU-bound dans un pool blocking **
                            let (create_evt, dev_trade, follow) = {
                                // On clone les logs pour les déplacer dans la closure
                                let logs = extract_program_logs(tx);
                                task::spawn_blocking(move || {
                                    let mut c = None;
                                    let mut t = None;
                                    let mut f = Vec::with_capacity(logs.len());
                                    for raw in logs {
                                        if let Ok(evt) = decode_event(&raw) {
                                            match evt {
                                                ParsedEvent::Create(e) => c = Some(e),
                                                ParsedEvent::Trade(e)  => {
                                                    t = Some(e.clone());
                                                    f.push(e);
                                                }
                                            }
                                        }
                                    }
                                    (c, t, f)
                                })
                                .await
                                .expect("panic dans spawn_blocking")
                            };

                            // Buy dans la même slot
                            if let (Some(create), Some(trade)) = (create_evt, dev_trade) {
                                if trade.sol_amount <= 2_000_000_000 {
                                    let token_id = create.mint.to_string();
                                    let buy_tx = wallet
                                        .buy_transaction(
                                            &create.mint,
                                            &create.bonding_curve,
                                            0.001,
                                            0.1,
                                            trade.clone(),
                                            blockhash,
                                        )
                                        .await
                                        .unwrap();

                                    // On logge la signature avant envoi
                                    let sig = buy_tx.signatures.get(0)
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| "<no-signature>".into());
                                    info!("🛒 Envoi Buy tx (sig={})", sig);

                                    // Envoi et log du résultat
                                    if let Err(e) = wallet
                                        .rpc_client
                                        .send_transaction_with_config(&buy_tx, rpc_conf).await
                                    {
                                        error!("❌ Buy failed for {} (sig={}): {:?}", token_id, sig, e);
                                        return;
                                    } else {
                                        info!("✅ Buy succeeded for {} (sig={})", token_id, sig);
                                    }

                                    // Création du worker si nécessaire
                                    if created.insert(token_id.clone(), ()).is_none() {
                                        manager.ensure_worker(&token_id);
                                        info!("🆕 Worker créé pour {}", token_id);
                                    }
                                }
                            }

                            // Routage des trades suivants
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
                    }
                }
            })
            .await;
        });
    }

    // 10) Log des tx de votre wallet
    {
        let mut ws = ReceiverStream::new(wallet_rx);
        tokio::spawn(async move {
            while let Some(tx_arc) = ws.next().await {
                info!("🟡 Wallet tx: {:?}", tx_arc);
            }
        });
    }

    // 11) Shutdown propre
    signal::ctrl_c().await?;
    info!("🛑 Fin du bot");
    Ok(())
}
