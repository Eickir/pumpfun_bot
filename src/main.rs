use dotenv::dotenv;
use std::{env, sync::Arc, str::FromStr};
use anyhow::Result;
use tracing::{info, error};
use tokio::{signal, task};
use tokio::sync::mpsc;
use solana_sdk::{hash::Hash, signature::Signature};
use solana_sdk::signer::keypair::Keypair;
use dashmap::DashMap;
use chrono::Local;
use arc_swap::ArcSwap;
use crossbeam::channel::{unbounded, Receiver as CbReceiver, Sender as CbSender};
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

struct DecodeJob {
    slot:      u64,
    tx_id:     Signature,
    tx_index:  u64,
    logs:      Vec<Vec<u8>>,
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
    // 1) Logger niveau INFO
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    dotenv().ok();
    info!("🚀 Démarrage du bot gRPC…");

    // 2) Config RPC Solana
    let rpc_config = RpcSendTransactionConfig {
        skip_preflight:      true,
        preflight_commitment: None,
        max_retries:         Some(0),
        encoding:            None,
        min_context_slot:    None,
    };

    // 3) Wallet setup
    let sk      = env::var("SOLANA_PRIVATE_KEY")?;
    let bytes   = bs58::decode(sk).into_vec()?;
    let keypair = Keypair::from_bytes(&bytes)?;
    let rpc_url = env::var("RPC_ENDPOINT")?;
    let wallet  = Arc::new(Wallet::new(keypair, &rpc_url));
    let balance = wallet.get_balance().await.unwrap();
    info!("✅ Wallet connecté, SOL disponible : {:.6} SOL", balance as f64 / 1e9);

    // 4) gRPC client
    let endpoint = env::var("GRPC_ENDPOINT")?;
    let x_token  = env::var("X_TOKEN")?;
    let client   = Arc::new(Client::new(endpoint, x_token)?);

    // 5) Channels Tokio pour blockmeta, pump, wallet-tx et résultats de décodage
    let (blockmeta_tx, mut blockmeta_rx)   = mpsc::channel::<Arc<SubscribeUpdateBlockMeta>>(1024);
    let (pump_tx,      mut pump_rx)        = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (wallet_tx,    _wallet_rx)         = mpsc::channel::<Arc<SubscribeUpdateTransaction>>(1024);
    let (decode_res_tx, mut decode_res_rx) = mpsc::channel::<DecodeResult>(1024);

    // 6) Lancement des streams gRPC
    {
        let client    = Arc::clone(&client);
        let bm_chan   = Arc::new(blockmeta_tx);
        let pump_chan = Arc::new(pump_tx);
        let wal_chan  = Arc::new(wallet_tx);
        info!("🔗 Lancement des streams gRPC");
        tokio::spawn(async move {
            client.subscribe_two_streams(bm_chan, pump_chan, wal_chan, 2000).await;
        });
    }

    // 7) Shared BlockMeta fallback
    let initial_meta = BlockMetaEntry {
        slot:        0,
        detected_at: Local::now(),
        blockhash:   Hash::default(),
    };
    let shared_blockmeta: SharedBlockMeta =
        Arc::new(ArcSwap::new(Arc::new(initial_meta)));

    // 8) Worker CPU-bound de décodage via crossbeam
    let (decode_req_tx, decode_req_rx): (CbSender<DecodeJob>, CbReceiver<DecodeJob>) = unbounded();
    {
        let decode_res_tx = decode_res_tx.clone();
        std::thread::spawn(move || {
            for job in decode_req_rx.iter() {
                let mut c = None;
                let mut t = None;
                let mut f = Vec::with_capacity(job.logs.len());
                for raw in job.logs {
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
                let res = DecodeResult {
                    slot:      job.slot,
                    tx_id:     job.tx_id,
                    tx_index:  job.tx_index,
                    create:    c,
                    dev_trade: t,
                    follow:    f,
                };
                let _ = decode_res_tx.try_send(res);
            }
        });
    }

    // 9) Pipeline principal « réception → décodage → envoi »
    {
        let shared   = Arc::clone(&shared_blockmeta);
        let manager  = Arc::new(TokenWorkerManager::new(1_000));
        let created  = Arc::new(DashMap::<String, ()>::new());
        let wallet   = Arc::clone(&wallet);
        let rpc_conf = rpc_config.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // ─── Mise à jour du BlockMeta
                    Some(bm_arc) = blockmeta_rx.recv() => {
                        if let Ok(h) = Hash::from_str(&bm_arc.blockhash) {
                            shared.store(Arc::new(BlockMetaEntry {
                                slot:        bm_arc.slot,
                                detected_at: Local::now(),
                                blockhash:   h,
                            }));
                        }
                    }

                    // ─── Traitement des résultats de décodage
                    Some(res) = decode_res_rx.recv() => {
                        // 1) Buy si Create+Trade
                        if let (Some(create), Some(trade)) = (res.create, res.dev_trade) {
                            if trade.sol_amount >= 500_000_000
                            && trade.sol_amount <= 5_000_000_000{
                                let token_id = create.mint.to_string();
                                // clone token_id pour la closure afin de conserver l'original
                                let token_for_spawn = token_id.clone();
                                let shared_hash     = shared.load().blockhash;
                                let wallet2         = Arc::clone(&wallet);
                                let rpc_conf2       = rpc_conf.clone();

                                task::spawn(async move {
                                    let buy_tx = wallet2
                                        .buy_transaction(
                                            &create.mint,
                                            &create.bonding_curve,
                                            0.001,
                                            0.1,
                                            trade,
                                            shared_hash,
                                        )
                                        .await
                                        .unwrap();
                                    let sig = buy_tx.signatures[0];
                                    match wallet2
                                        .rpc_client
                                        .send_transaction_with_config(&buy_tx, rpc_conf2)
                                        .await
                                    {
                                        Ok(_) => info!("✅ Buy succeeded for {} (sig={})", token_for_spawn, sig),
                                        Err(e) => error!("❌ Buy failed for {} (sig={}): {:?}", token_for_spawn, sig, e),
                                    }
                                });

                                // utilisation de token_id ici encore
                                if created.insert(token_id.clone(), ()).is_none() {
                                    manager.ensure_worker(&token_id);
                                }
                            }
                        }
                        // 2) Routage des trades suivants
                        for e in res.follow {
                            let tid = e.mint.to_string();
                            if created.contains_key(&tid) {
                                manager.route_trade(
                                    &tid,
                                    EnrichedTradeEvent {
                                        trade:    e.clone(),
                                        tx_id:    res.tx_id.clone(),
                                        slot:     res.slot,
                                        tx_index: res.tx_index,
                                    },
                                )
                                .await;
                            }
                        }
                    }

                    // ─── Réception d’une transaction PumpFun → dispatch décodage
                    Some(tx_arc) = pump_rx.recv() => {
                        let tx = &*tx_arc;
                        if let Some(info) = tx.transaction.as_ref() {
                            let sig  = Signature::try_from(info.signature.clone()).unwrap();
                            let job  = DecodeJob {
                                slot:     tx.slot,
                                tx_id:    sig,
                                tx_index: info.index,
                                logs:     extract_program_logs(tx),
                            };
                            let _    = decode_req_tx.send(job);
                        }
                    }

                    // ─── Aucun flux → sortie
                    else => break,
                }
            }
        });
    }

    // 10) Ctrl-C → shutdown
    signal::ctrl_c().await?;
    info!("🛑 Fin du bot");
    Ok(())
}
