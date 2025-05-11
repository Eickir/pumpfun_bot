use dotenv::dotenv;
use std::{env, sync::Arc};
use anyhow::Result;
use tracing::{info, error};
use tokio::signal;
use solana_sdk::{hash::Hash, signature::Signature, signer::Signer};
use solana_sdk::signer::keypair::Keypair;
use dashmap::DashMap;
use flume::Receiver;
use chrono::{Local};
use arc_swap::ArcSwap;
use yellowstone_grpc_proto::prelude::{SubscribeUpdateBlockMeta, SubscribeUpdateTransaction};
use solana_client::rpc_config::RpcSendTransactionConfig;

mod modules;
use crate::modules::{
    grpc_configuration::client::Client,
    token_manager::token_manager::TokenWorkerManager,
    utils::decoder::{extract_program_logs, decode_event},
    utils::types::{ParsedEvent, EnrichedTradeEvent},
    utils::blockmeta_handler::{BlockMetaEntry, SharedBlockMeta, spawn_blockmeta_data},
    wallet::wallet::Wallet,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // 1) Logger + .env
    tracing_subscriber::fmt().init();
    dotenv().ok();

    // 2) Config RPC Solana
    let config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: None,
        max_retries: Some(0),
        encoding: None,
        min_context_slot: None,
    };

    info!("🚀 Démarrage du bot gRPC…");

    // 3) Wallet
    let private_key_str = env::var("SOLANA_PRIVATE_KEY")?;
    let keypair_bytes = bs58::decode(private_key_str).into_vec()?;
    let keypair = Keypair::from_bytes(&keypair_bytes)?;
    let rpc_url = env::var("RPC_ENDPOINT")?;
    let wallet = Arc::new(Wallet::new(keypair, &rpc_url));
    let balance = wallet.get_balance().await.unwrap();
    info!(
        "Wallet connecté, SOL disponible: {}",
        balance as f64 / 1_000_000_000.0
    );

    // 4) gRPC client : prépare aussi la SubscribeRequest
    let endpoint = env::var("GRPC_ENDPOINT")?;
    let x_token = env::var("X_TOKEN")?;
    let client = Arc::new(Client::new(endpoint, x_token)?);

    // 5) Canaux Flume pour blockmeta & tx
    let (blockmeta_tx, blockmeta_rx) = flume::unbounded::<SubscribeUpdateBlockMeta>();
    let (pumpfun_tx, pumpfun_rx) = flume::unbounded::<SubscribeUpdateTransaction>();
    let (wallet_tx, wallet_rx) = flume::unbounded::<SubscribeUpdateTransaction>();

    // 6) Lancement de la souscription gRPC en tâche de fond
    {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .subscribe_with_reconnect(
                    Arc::new(blockmeta_tx),
                    Arc::new(pumpfun_tx),
                    Arc::new(wallet_tx),
                    1000, // concurrency_limit
                )
                .await;
        });
    }

    // 7) Stockage atomique du blockmeta
    let initial_meta = BlockMetaEntry {
        slot: 0,
        detected_at: Local::now(),
        blockhash: Hash::default(),
    };
    let shared_blockmeta: SharedBlockMeta = Arc::new(ArcSwap::new(Arc::new(initial_meta)));

    // 8) Tâche de persistance blockmeta
    {
        let storage = Arc::clone(&shared_blockmeta);
        tokio::spawn(async move {
            spawn_blockmeta_data(blockmeta_rx, storage).await;
        });
    }

    // 9) Manager PumpFun
    let manager = Arc::new(TokenWorkerManager::new(1_000));
    let created_tokens = Arc::new(DashMap::<String, ()>::new());

    // 10) Log des tx de mon wallet
    {
        tokio::spawn(async move {
            while let Ok(tx) = wallet_rx.recv_async().await {
                info!("🟡 Wallet tx: {:?}", tx);
            }
        });
    }

    // 11) Traitement PumpFun (Create + Trade) par worker
    {
        let manager = Arc::clone(&manager);
        let created_tokens = Arc::clone(&created_tokens);
        let shared_blockmeta = Arc::clone(&shared_blockmeta);
        let wallet = Arc::clone(&wallet);

        tokio::spawn(async move {
            while let Ok(tx) = pumpfun_rx.recv_async().await {
                let slot = tx.slot;
                // signature + index
                let (tx_id, tx_index) = if let Some(info) = tx.transaction.as_ref() {
                    let sig = Signature::try_from(info.signature.clone()).unwrap();
                    (sig, info.index)
                } else {
                    continue;
                };

                // blockhash à jour
                let recent_blockhash = shared_blockmeta.load().blockhash;

                // 1) détection Create + Trade du dev
                let mut maybe_create = None;
                let mut maybe_dev_trade = None;
                for raw in extract_program_logs(&tx) {
                    match decode_event(&raw) {
                        Ok(ParsedEvent::Create(evt)) => maybe_create = Some(evt.clone()),
                        Ok(ParsedEvent::Trade(evt)) => maybe_dev_trade = Some(evt.clone()),
                        _ => {}
                    }
                }

                // 2) Buy si ready
                if let (Some(create_evt), Some(trade_evt)) = (maybe_create, maybe_dev_trade) {
                    if trade_evt.sol_amount <= 2_000_000_000 {
                        let token_id = create_evt.mint.to_string();
                        let tx = wallet
                            .buy_transaction(
                                &create_evt.mint,
                                &create_evt.bonding_curve,
                                0.001,
                                0.1,
                                trade_evt.clone(),
                                recent_blockhash,
                            )
                            .await
                            .unwrap();

                        match wallet.rpc_client.send_transaction_with_config(&tx, config).await {
                            Ok(sig) => info!("✅ Buy tx pour {}: {:?}", token_id, sig),
                            Err(e) => {
                                error!("❌ Échec du buy pour {}: {:?}", token_id, e);
                                continue;
                            }
                        }

                        if created_tokens.insert(token_id.clone(), ()).is_none() {
                            manager.ensure_worker(&token_id);
                            info!("🆕 Worker créé pour {}", token_id);
                        }
                    }
                }

                // 3) Routage des trades suivants
                for raw in extract_program_logs(&tx) {
                    if let Ok(ParsedEvent::Trade(evt)) = decode_event(&raw) {
                        let token_id = evt.mint.to_string();
                        if created_tokens.contains_key(&token_id) {
                            let enriched = EnrichedTradeEvent {
                                trade: evt.clone(),
                                tx_id: tx_id.clone(),
                                slot,
                                tx_index,
                            };
                            manager.route_trade(&token_id, enriched).await;
                        }
                    }
                }
            }
        });
    }

    // 12) Shutdown propre
    signal::ctrl_c().await?;
    info!("🛑 Fin du bot");
    Ok(())
}
