// src/modules/grpc_configuration/client.rs

use std::{collections::HashMap, sync::Arc, env};
use solana_sdk::signer::{keypair::Keypair, Signer};
use tokio::time::{sleep, Duration};
use tokio::sync::mpsc::Sender;
use tonic::transport::ClientTlsConfig;
use tracing::{info, error};
use futures_util::StreamExt;
use yellowstone_grpc_client::{GeyserGrpcClient, Interceptor};
use once_cell::sync::Lazy;
use yellowstone_grpc_proto::prelude::*;
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;

// vos constantes de configuration gRPC
use crate::modules::grpc_configuration::constants::{
    CONNECT_TIMEOUT, REQUEST_TIMEOUT, KEEP_ALIVE_INTERVAL, KEEP_ALIVE_TIMEOUT,
    MAX_MSG_SIZE, PUMPFUN_PROGRAM_ID,
};

// TLS partagé pour éviter de récharger à chaque connexion
static TLS_CONFIG: Lazy<ClientTlsConfig> =
    Lazy::new(|| ClientTlsConfig::new().with_native_roots());

#[derive(Clone, Debug)]
pub struct Client {
    endpoint: String,
    x_token: String,
    base_request: SubscribeRequest,
}

impl Client {
    pub fn new(endpoint: String, x_token: String) -> anyhow::Result<Self> {
        // charge et décode la clé une seule fois
        let sk = env::var("SOLANA_PRIVATE_KEY")?;
        let bytes = bs58::decode(sk).into_vec()?;
        let pubkey = Keypair::from_bytes(&bytes)?.pubkey();

        // construit le map `transactions`
        let mut transactions = HashMap::new();
        transactions.insert(
            "pumpfun_listener".into(),
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
            "my_wallet_transaction_listener".into(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: None,
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![],
                account_required: vec![pubkey.to_string()],
            },
        );

        // filtre pour les block meta
        let mut blocks_meta = HashMap::new();
        blocks_meta.insert("block_listener".into(), SubscribeRequestFilterBlocksMeta {});

        let base_request = SubscribeRequest {
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

        Ok(Self { endpoint, x_token, base_request })
    }

    async fn connect(&self) -> anyhow::Result<GeyserGrpcClient<impl Interceptor>> {
        GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
            .x_token(Some(self.x_token.clone()))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .tls_config((*TLS_CONFIG).clone())?
            .max_decoding_message_size(MAX_MSG_SIZE)
            // ← Activation de TCP_NODELAY
            .tcp_nodelay(true)
            // ← Augmentation des fenêtres HTTP/2 pour réduire le buffering
            .initial_stream_window_size(1 << 20)        // 1 MiB
            .initial_connection_window_size(1 << 20)    // 1 MiB
            .connect()
            .await
            .map_err(Into::into)
    }

    /// Ouvre deux flux gRPC en parallèle :
    /// - pumpfun + blockmeta
    /// - my_wallet_transaction
    pub async fn subscribe_two_streams(
        self: Arc<Self>,
        blockmeta_tx: Arc<Sender<Arc<SubscribeUpdateBlockMeta>>>,
        pump_tx: Arc<Sender<Arc<SubscribeUpdateTransaction>>>,
        wallet_tx: Arc<Sender<Arc<SubscribeUpdateTransaction>>>,
        concurrency_limit: usize,
    ) {
        // prépare deux requêtes filtrées
        let mut pump_req = self.base_request.clone();
        pump_req.transactions.retain(|k, _| k == "pumpfun_listener");
        // on garde blockmeta pour pump_req

        let mut wallet_req = self.base_request.clone();
        wallet_req.transactions.retain(|k, _| k == "my_wallet_transaction_listener");
        wallet_req.blocks_meta = HashMap::new(); // plus de blockmeta pour wallet

        let svc = Arc::clone(&self);

        // flux pumpfun + blockmeta
        {
            let svc = Arc::clone(&svc);
            let pump_req = pump_req.clone();
            let blockmeta_tx = Arc::clone(&blockmeta_tx);
            let pump_tx = Arc::clone(&pump_tx);

            tokio::spawn(async move {
                loop {
                    match svc.connect().await {
                        Ok(mut client) => {
                            if let Ok((_sink, mut stream)) =
                                client.subscribe_with_request(Some(pump_req.clone())).await
                            {
                                info!("📡 pumpfun stream subscribed");
                                stream
                                    .for_each_concurrent(concurrency_limit, |res| {
                                        let blockmeta_tx = Arc::clone(&blockmeta_tx);
                                        let pump_tx = Arc::clone(&pump_tx);
                                        async move {
                                            if let Ok(update) = res {
                                                // emprunt pour BlockMeta
                                                if let Some(UpdateOneof::BlockMeta(bm)) =
                                                    update.update_oneof.as_ref()
                                                {
                                                    let _ = blockmeta_tx
                                                        .try_send(Arc::new(bm.clone()));
                                                }
                                                // emprunt pour Transaction
                                                if let Some(UpdateOneof::Transaction(tx)) =
                                                    update.update_oneof.as_ref()
                                                {
                                                    let _ = pump_tx
                                                        .try_send(Arc::new(tx.clone()));
                                                }
                                            }
                                        }
                                    })
                                    .await;
                            }
                        }
                        Err(err) => error!("pumpfun connect failed: {}", err),
                    }
                    sleep(Duration::from_secs(5)).await;
                }
            });
        }

        // flux wallet
        {
            let svc = Arc::clone(&svc);
            let wallet_req = wallet_req.clone();
            let wallet_tx = Arc::clone(&wallet_tx);

            tokio::spawn(async move {
                loop {
                    match svc.connect().await {
                        Ok(mut client) => {
                            if let Ok((_sink, mut stream)) =
                                client.subscribe_with_request(Some(wallet_req.clone())).await
                            {
                                info!("📡 wallet stream subscribed");
                                stream
                                    .for_each_concurrent(concurrency_limit, |res| {
                                        let wallet_tx = Arc::clone(&wallet_tx);
                                        async move {
                                            if let Ok(update) = res {
                                                if let Some(UpdateOneof::Transaction(tx)) =
                                                    update.update_oneof.as_ref()
                                                {
                                                    let _ = wallet_tx
                                                        .try_send(Arc::new(tx.clone()));
                                                }
                                            }
                                        }
                                    })
                                    .await;
                            }
                        }
                        Err(err) => error!("wallet connect failed: {}", err),
                    }
                    sleep(Duration::from_secs(5)).await;
                }
            });
        }
    }
}
