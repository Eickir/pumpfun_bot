use solana_sdk::signer::Signer;
use tonic::transport::ClientTlsConfig;
use yellowstone_grpc_client::{GeyserGrpcClient, Interceptor};
use once_cell::sync::Lazy;
use yellowstone_grpc_proto::prelude::*;
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;
use futures_util::StreamExt;
use tracing::{error, info};
use tonic::Status;
use flume::Sender;
use std::{collections::HashMap, sync::Arc};
use tokio::time::{sleep, Duration};
use std::env;

// Vos constantes de configuration gRPC
use crate::modules::grpc_configuration::constants::{
    CONNECT_TIMEOUT, REQUEST_TIMEOUT, KEEP_ALIVE_INTERVAL, KEEP_ALIVE_TIMEOUT,
    MAX_MSG_SIZE, PUMPFUN_PROGRAM_ID,
};

// Cache TLS global pour gagner du temps à chaque connexion
static TLS_CONFIG: Lazy<ClientTlsConfig> =
    Lazy::new(|| ClientTlsConfig::new().with_native_roots());

/// Groupe vos canaux pour ne cloner qu'un seul Arc par message
struct TxChannels {
    meta: Arc<Sender<SubscribeUpdateBlockMeta>>,
    pump: Arc<Sender<SubscribeUpdateTransaction>>,
    wallet: Arc<Sender<SubscribeUpdateTransaction>>,
}

#[derive(Debug, Clone)]
pub struct Client {
    endpoint: String,
    x_token: String,
    subscribe_request: SubscribeRequest,
}

impl Client {
    /// Crée un nouveau client et prépare la SubscribeRequest une fois pour toutes
    pub fn new(endpoint: String, x_token: String) -> anyhow::Result<Self> {
        // Décodez votre clé une seule fois
        let private_key_str = env::var("SOLANA_PRIVATE_KEY")?;
        let keypair_bytes = bs58::decode(private_key_str).into_vec()?;
        let pubkey = solana_sdk::signer::keypair::Keypair::from_bytes(&keypair_bytes)?.pubkey();

        // Construisez votre map de filtres transactions
        let mut transactions = HashMap::new();
        transactions.insert(
            "pumpfun_listener".to_string(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: Some(false),
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![pubkey.to_string()],
                account_required: vec![],
            },
        );
        transactions.insert(
            "my_wallet_transaction_listener".to_string(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: None,
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![],
                account_required: vec![pubkey.to_string()],
            },
        );

        // Map pour le BlockMeta
        let mut blocks_meta = HashMap::new();
        blocks_meta.insert("block_listener".to_string(), SubscribeRequestFilterBlocksMeta {});

        // Préparez la requête complète
        let subscribe_request = SubscribeRequest {
            slots: HashMap::new(),
            accounts: HashMap::new(),
            transactions,
            transactions_status: HashMap::new(),
            entry: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta,
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: Vec::new(),
            ping: None,
            from_slot: None,
        };

        Ok(Self { endpoint, x_token, subscribe_request })
    }

    /// Établit la connexion gRPC
    pub async fn connect(self: Arc<Self>) -> anyhow::Result<GeyserGrpcClient<impl Interceptor>> {
        GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
            .x_token(Some(self.x_token.clone()))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .tls_config((*TLS_CONFIG).clone())?
            .max_decoding_message_size(MAX_MSG_SIZE)
            .connect()
            .await
            .map_err(Into::into)
    }

    /// Boucle de reconnexion + traitement ultra-optimisé du flux gRPC
    pub async fn subscribe_with_reconnect(
        self: Arc<Self>,
        blockmeta_tx: Arc<Sender<SubscribeUpdateBlockMeta>>,
        pumpfun_tx:   Arc<Sender<SubscribeUpdateTransaction>>,
        wallet_tx:    Arc<Sender<SubscribeUpdateTransaction>>,
        concurrency_limit: usize,
    ) {
        // Regroupez vos canaux pour un seul clone d'Arc
        let txs = Arc::new(TxChannels {
            meta: blockmeta_tx,
            pump: pumpfun_tx,
            wallet: wallet_tx,
        });

        loop {
            match Arc::clone(&self).connect().await {
                Ok(mut client) => {
                    info!("✅ Connected to gRPC server.");
                    // Réutilisez la même requête à chaque reconnexion
                    let req = self.subscribe_request.clone();
                    match client.subscribe_with_request(Some(req)).await {
                        Ok((_sink, stream)) => {
                            info!("📡 Subscribed to gRPC stream.");

                            stream
                                // 1) Inspect synchronously pour les BlockMeta (pas de clone lourd)
                                .inspect(|maybe_update| {
                                    if let Ok(update) = maybe_update {
                                        if let Some(UpdateOneof::BlockMeta(bm)) = &update.update_oneof {
                                            // try_send non-bloquant
                                            let _ = txs.meta.try_send(bm.clone());
                                        }
                                    }
                                })
                                // 2) Ne gardez que les transactions à router, taguées wallet ou pump
                                .filter_map(|res| {
                                    let txs = Arc::clone(&txs);
                                    async move {
                                        match res {
                                            Ok(update) => {
                                                if let Some(UpdateOneof::Transaction(tx)) = update.update_oneof {
                                                    let is_wallet = update
                                                        .filters
                                                        .iter()
                                                        .any(|f| f == "my_wallet_transaction_listener");
                                                    Some((is_wallet, tx))
                                                } else {
                                                    None
                                                }
                                            }
                                            Err(err) => {
                                                error!("🔴 gRPC stream error: {:?}", err);
                                                None
                                            }
                                        }
                                    }
                                })
                                // 3) Routez en mémoire en parallèle jusqu'à concurrence_limit
                                .for_each_concurrent(concurrency_limit, |(is_wallet, tx)| {
                                    let txs = Arc::clone(&txs);
                                    async move {
                                        if is_wallet {
                                            let _ = txs.wallet.try_send(tx);
                                        } else {
                                            let _ = txs.pump.try_send(tx);
                                        }
                                    }
                                })
                                .await;
                        }
                        Err(err) => error!("❌ Subscription failed: {}", err),
                    }
                }
                Err(err) => error!("❌ Connection failed: {}", err),
            }
            info!("⏳ Reconnecting in 5 seconds…");
            sleep(Duration::from_secs(5)).await;
        }
    }
}
