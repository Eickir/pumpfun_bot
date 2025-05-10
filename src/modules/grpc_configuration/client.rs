use tonic::transport::ClientTlsConfig;
use yellowstone_grpc_client::{GeyserGrpcClient, Interceptor};
use once_cell::sync::Lazy;
use yellowstone_grpc_proto::prelude::*;
use std::collections::HashMap;
use crate::modules::grpc_configuration::constants::{
    CONNECT_TIMEOUT, REQUEST_TIMEOUT, KEEP_ALIVE_INTERVAL, KEEP_ALIVE_TIMEOUT,
    MAX_MSG_SIZE, PUMPFUN_PROGRAM_ID,
};
use solana_sdk::signer::{keypair::Keypair, Signer};
use anyhow::Result;
use futures_util::StreamExt;
use tracing::{error, info};
use tonic::Status;
use flume::Sender;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;
use std::env;

// Cache du TLS pour améliorer les perfs
static TLS_CONFIG: Lazy<ClientTlsConfig> =
    Lazy::new(|| ClientTlsConfig::new().with_native_roots());

#[derive(Debug, Clone)]
pub struct Client {
    pub endpoint: String,
    pub x_token: String,
}

impl Client {
    /// Crée un nouveau client
    pub fn new(endpoint: String, x_token: String) -> Self {
        Self { endpoint, x_token }
    }

    /// Connexion gRPC
    pub async fn connect(self: Arc<Self>) -> Result<GeyserGrpcClient<impl Interceptor>> {
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

    /// Souscription unique pour PumpFun + wallet
    pub fn sniping_bot_listener(&self) -> Result<SubscribeRequest> {
        let mut transactions = HashMap::new();


        // Votre wallet seul
        let private_key_str = env::var("SOLANA_PRIVATE_KEY")?;
        let keypair_bytes = bs58::decode(private_key_str).into_vec()?;
        let keypair = Keypair::from_bytes(&keypair_bytes)?;
        let pubkey = keypair.pubkey();

        // Toutes les tx PumpFun
        transactions.insert(
            "pumpfun_listener".to_owned(),
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
            "my_wallet_transaction_listener".to_owned(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: None,
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![],
                account_required: vec![pubkey.to_string()],
            },
        );

        let mut blocks_meta = HashMap::new();
        blocks_meta.insert(
            "block_listener".to_owned(),
            SubscribeRequestFilterBlocksMeta {},
        );

        info!("🔄 Reconnexion avec from_slot: None");

        Ok(SubscribeRequest {
            slots: HashMap::default(),
            accounts: HashMap::default(),
            transactions,
            transactions_status: HashMap::default(),
            entry: HashMap::default(),
            blocks: HashMap::default(),
            blocks_meta,
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: Vec::new(),
            ping: None,
            from_slot: None,
        })
    }

    /// Boucle de reconnexion + subscription
    pub async fn subscribe_with_reconnect(
        self: Arc<Self>,
        blockmeta_tx: Arc<Sender<SubscribeUpdateBlockMeta>>,
        pumpfun_tx:   Arc<Sender<SubscribeUpdateTransaction>>,
        wallet_tx:    Arc<Sender<SubscribeUpdateTransaction>>,
        concurrency_limit: usize,
    ) {
        loop {
            match Arc::clone(&self).connect().await {
                Ok(mut client) => {
                    info!("✅ Connected to GRPC server.");
                    let req = self.sniping_bot_listener().unwrap();
                    match client.subscribe_with_request(Some(req)).await {
                        Ok((_sink, stream)) => {
                            info!("📡 Subscribed to GRPC stream.");
                            self.handle_stream(
                                stream,
                                blockmeta_tx.clone(),
                                pumpfun_tx.clone(),
                                wallet_tx.clone(),
                                concurrency_limit,
                            )
                            .await;
                        }
                        Err(e) => error!("❌ Subscription failed: {}", e),
                    }
                }
                Err(e) => error!("❌ Connection failed: {}", e),
            }
            info!("⏳ Reconnecting in 5 seconds…");
            sleep(Duration::from_secs(5)).await;
        }
    }

    /// Gère un flux unique et distribue vers 3 canaux
    async fn handle_stream(
        &self,
        receiver: impl StreamExt<Item = Result<SubscribeUpdate, Status>> + Unpin,
        blockmeta_tx: Arc<Sender<SubscribeUpdateBlockMeta>>,
        pumpfun_tx:   Arc<Sender<SubscribeUpdateTransaction>>,
        wallet_tx:    Arc<Sender<SubscribeUpdateTransaction>>,
        concurrency_limit: usize,
    ) {
        use futures_util::StreamExt;
    
        receiver
            .map(|maybe_update| {
                let blockmeta_tx = blockmeta_tx.clone();
                let pumpfun_tx   = pumpfun_tx.clone();
                let wallet_tx    = wallet_tx.clone();
    
                async move {
                    if let Ok(update) = maybe_update {
                        // 1) BlockMeta
                        if let Some(UpdateOneof::BlockMeta(bm)) =
                            update.update_oneof.clone()
                        {
                            let _ = blockmeta_tx.send_async(bm).await;
                        }
                        // 2) Transaction
                        if let Some(UpdateOneof::Transaction(tx)) =
                            update.update_oneof
                        {
                            // Choix du canal selon filters
                            if update.filters.iter().any(|f| f == "my_wallet_transaction_listener") {
                                let _ = wallet_tx.send_async(tx).await;
                            } else {
                                let _ = pumpfun_tx.send_async(tx).await;
                            }
                        }
                    }
                }
            })
            // Concurrence sur le .send_async mais rendu en ordre d'origine
            .buffered(concurrency_limit)
            // on consomme les futures renvoyées par `map`
            .for_each(|_| async {})
            .await;
    }
    
}
