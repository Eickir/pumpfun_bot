use dotenv::dotenv;
use std::{env, sync::Arc};
use anyhow::Result;
use tracing::info;
use tokio::signal;
use solana_sdk::signature::Signature;
use dashmap::DashMap;

mod modules;
use crate::modules::{
    grpc_configuration::client::Client,
    token_manager::token_manager::TokenWorkerManager,
    utils::decoder::{extract_program_logs, decode_event},
};
use crate::modules::utils::types::{ParsedEvent, EnrichedTradeEvent};
use yellowstone_grpc_proto::prelude::{
    SubscribeUpdateBlockMeta, SubscribeUpdateTransaction,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // 1) Logger + .env
    tracing_subscriber::fmt().init();
    dotenv().ok();
    info!("🚀 Démarrage du bot gRPC…");

    // 2) Récupère endpoint & token d'env
    let endpoint = env::var("GRPC_ENDPOINT")
        .expect("GRPC_ENDPOINT doit être défini");
    let x_token = env::var("X_TOKEN")
        .expect("X_TOKEN doit être défini");

    // 3) Crée le client gRPC
    let client = Arc::new(Client::new(endpoint, x_token));

    // 4) Crée les canaux flume
    let (blockmeta_tx, blockmeta_rx) =
        flume::unbounded::<SubscribeUpdateBlockMeta>();
    let (pumpfun_tx, pumpfun_rx) =
        flume::unbounded::<SubscribeUpdateTransaction>();
    let (wallet_tx, wallet_rx) =
        flume::unbounded::<SubscribeUpdateTransaction>();

    // 5) Lance la souscription gRPC en arrière-plan
    {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client.subscribe_with_reconnect(
                Arc::new(blockmeta_tx),
                Arc::new(pumpfun_tx),
                Arc::new(wallet_tx),
                1000,
            ).await;
        });
    }

    // 6) Manager PumpFun
    let manager = Arc::new(TokenWorkerManager::new(1_000));
    let created_tokens = Arc::new(DashMap::<String, ()>::new());

    // 7.a) Lecture BlockMeta
    {
        tokio::spawn(async move {
            while let Ok(bm) = blockmeta_rx.recv_async().await {
                info!("🔷 BlockMeta slot={}", bm.slot);
            }
        });
    }

    // 7.b) Lecture Wallet
    {
        tokio::spawn(async move {
            while let Ok(tx) = wallet_rx.recv_async().await {
                let sig = tx.transaction
                    .as_ref()
                    .map(|info| info.signature.clone());
                info!("🟡 Wallet tx: {:?}", sig);
            }
        });
    }

    // 7.c) Lecture PumpFun (Create + Trade)
    {
        let manager = Arc::clone(&manager);
        let created_tokens = Arc::clone(&created_tokens);
        tokio::spawn(async move {
            while let Ok(tx) = pumpfun_rx.recv_async().await {
                let slot = tx.slot;
                // signature + index
                let (tx_id, tx_index) = if let Some(info) = tx.transaction.as_ref() {
                    let sig = Signature::try_from(info.signature.clone())
                        .expect("invalid signature bytes");
                    (sig, info.index)
                } else {
                    continue;
                };

                // decode logs
                for raw in extract_program_logs(&tx) {
                    match decode_event(&raw) {
                        Ok(ParsedEvent::Create(evt)) => {
                            let token_id = evt.mint.to_string();
                            info!("🆕 Token créé: {}", token_id);
                            created_tokens.insert(token_id.clone(), ());
                            manager.ensure_worker(&token_id);
                        }
                        Ok(ParsedEvent::Trade(trade_evt)) => {
                            let token_id = trade_evt.mint.to_string();
                            if created_tokens.contains_key(&token_id) {
                                let enriched = EnrichedTradeEvent {
                                    trade: trade_evt.clone(),
                                    tx_id: tx_id.clone(),
                                    slot,
                                    tx_index,
                                };
                                // envoi async au worker
                                manager.route_trade(&token_id, enriched).await;
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    // 8) Attente Ctrl+C
    signal::ctrl_c().await?;
    info!("🛑 Fin du bot");
    Ok(())
}
