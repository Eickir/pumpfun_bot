//! modules/grpc_configuration/client.rs
//! v2 – publie un signal quand le flux **wallet** est abonné
//!      (pour que `main` puisse attendre ce « go » avant d’analyser Pump.fun)

use std::{
    collections::HashMap,
    env,
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use tokio::{
    sync::{mpsc::Sender, oneshot},
    time::sleep,
};
use tonic::transport::ClientTlsConfig;
use tracing::{error, info, debug};
use yellowstone_grpc_client::{GeyserGrpcClient, Interceptor};
use yellowstone_grpc_proto::prelude::*;
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;

use solana_sdk::signer::{keypair::Keypair, Signer};

use crate::modules::grpc_configuration::constants::{
    CONNECT_TIMEOUT, KEEP_ALIVE_INTERVAL, KEEP_ALIVE_TIMEOUT, MAX_MSG_SIZE, PUMPFUN_PROGRAM_ID,
    REQUEST_TIMEOUT,
};

// TLS racine partagé
static TLS_CONFIG: Lazy<ClientTlsConfig> = Lazy::new(|| ClientTlsConfig::new().with_native_roots());

/// Alias pratique
type TxSub<T> = Arc<Sender<Arc<T>>>;

// ---------------------------------------------------------------------------
/// Client gRPC Yellowstone
// ---------------------------------------------------------------------------
#[derive(Clone)]
pub struct Client {
    endpoint:   String,
    x_token:    String,
    pump_req:   SubscribeRequest,
    wallet_req: SubscribeRequest,
    wallet_ready_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>, // ← nouveau
}

impl Client {
    // -----------------------------------------------------------------------
    /// Construit le client et prépare les deux `SubscribeRequest`
    // -----------------------------------------------------------------------
    pub fn new(endpoint: String, x_token: String) -> anyhow::Result<Self> {
        // 1. pubkey du wallet
        let bytes  = bs58::decode(env::var("SOLANA_PRIVATE_KEY")?).into_vec()?;
        let pubkey = Keypair::from_bytes(&bytes)?.pubkey().to_string();

        // 2. Filtres Pump.fun et wallet
        let mut pump_filter = HashMap::new();
        pump_filter.insert(
            "pumpfun_listener".into(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: Some(false),
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![pubkey.clone()],
                account_required: vec![],
            },
        );

        let mut wallet_filter = HashMap::new();
        wallet_filter.insert(
            "my_wallet_listener".into(),
            SubscribeRequestFilterTransactions {
                vote: None,
                failed: None,
                signature: None,
                account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
                account_exclude: vec![],
                account_required: vec![pubkey],
            },
        );

        // 3. BlockMeta pour Pump.fun
        let mut blocks_meta = HashMap::new();
        blocks_meta.insert("block_listener".into(), SubscribeRequestFilterBlocksMeta {});

        // 4. Gabarit
        let template = || SubscribeRequest {
            slots:               HashMap::new(),
            accounts:            HashMap::new(),
            transactions:        HashMap::new(),
            transactions_status: HashMap::new(),
            entry:               HashMap::new(),
            blocks:              HashMap::new(),
            blocks_meta:         HashMap::new(),
            commitment:          Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: vec![],
            ping:                None,
            from_slot:           None,
        };

        let pump_req = {
            let mut r = template();
            r.transactions = pump_filter;
            r.blocks_meta  = blocks_meta;
            r
        };

        let wallet_req = {
            let mut r = template();
            r.transactions = wallet_filter;
            r
        };

        Ok(Self {
            endpoint,
            x_token,
            pump_req,
            wallet_req,
            wallet_ready_tx: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Le `main` injecte ici le `oneshot::Sender` à notifier
    pub async fn set_wallet_ready_notifier(&self, tx: oneshot::Sender<()>) {
        let mut guard = self.wallet_ready_tx.lock().await;
        *guard = Some(tx);
    }

    // -----------------------------------------------------------------------
    /// Ouvre la connexion gRPC avec tous les réglages
    // -----------------------------------------------------------------------
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
            .tcp_nodelay(true)
            .initial_stream_window_size(1 << 23)
            .initial_connection_window_size(1 << 23)
            .connect()
            .await
            .map_err(Into::into)
    }

    // -----------------------------------------------------------------------
    /// Lance deux connexions : PumpFun(+BlockMeta) et Wallet
    // -----------------------------------------------------------------------
    pub async fn subscribe_two_streams(
        self: Arc<Self>,
        blockmeta_tx: TxSub<SubscribeUpdateBlockMeta>,
        pump_tx:      TxSub<SubscribeUpdateTransaction>,
        wallet_tx:    TxSub<SubscribeUpdateTransaction>,
        concurrency: usize,
    ) {
        // ========== 1. PumpFun + BlockMeta ==================================
        {
            let me   = Arc::clone(&self);
            let pump = Arc::clone(&pump_tx);
            let bm   = Arc::clone(&blockmeta_tx);

            tokio::spawn(async move {
                let mut backoff = 0;
                loop {
                    match me.connect().await {
                        Ok(mut client) => {
                            backoff = 0;
                            if let Ok((_sink, mut stream)) =
                                client.subscribe_with_request(Some(me.pump_req.clone())).await
                            {
                                debug!("📡 pumpfun stream subscribed");

                                stream
                                    .for_each_concurrent(concurrency, {
                                        let p = Arc::clone(&pump);
                                        let b = Arc::clone(&bm);
                                        move |res| {
                                            let p = Arc::clone(&p);
                                            let b = Arc::clone(&b);
                                            async move {
                                                match res {
                                                    Ok(u) => match u.update_oneof {
                                                        Some(UpdateOneof::BlockMeta(m)) => {
                                                            let _ = b.send(Arc::new(m)).await;
                                                        }
                                                        Some(UpdateOneof::Transaction(t)) => {
                                                            let _ = p.send(Arc::new(t)).await;
                                                        }
                                                        _ => {}
                                                    },
                                                    Err(e) => error!("pump item error: {e}"),
                                                }
                                            }
                                        }
                                    })
                                    .await;
                            }
                        }
                        Err(e) => {
                            error!("pump connection failed: {e}");
                            backoff = (backoff.max(1) * 2).min(32);
                            sleep(Duration::from_secs(backoff)).await;
                        }
                    }
                }
            });
        }

        // ========== 2. Wallet ==============================================
        {
            let me  = Arc::clone(&self);
            let wal = Arc::clone(&wallet_tx);

            tokio::spawn(async move {
                let mut backoff = 0;
                loop {
                    match me.connect().await {
                        Ok(mut client) => {
                            backoff = 0;
                            if let Ok((_sink, mut stream)) =
                                client.subscribe_with_request(Some(me.wallet_req.clone())).await
                            {
                                debug!("📡 wallet stream subscribed");
                                // → notifier le main si un Sender a été fourni
                                if let Some(tx) = me.wallet_ready_tx.lock().await.take() {
                                    let _ = tx.send(());
                                }

                                stream
                                    .for_each_concurrent(concurrency, {
                                        let w = Arc::clone(&wal);
                                        move |res| {
                                            let w = Arc::clone(&w);
                                            async move {
                                                match res {
                                                    Ok(u) => {
                                                        if let Some(UpdateOneof::Transaction(t)) = u.update_oneof {
                                                            let _ = w.send(Arc::new(t)).await;
                                                        }
                                                    }
                                                    Err(e) => error!("wallet item error: {e}"),
                                                }
                                            }
                                        }
                                    })
                                    .await;
                            }
                        }
                        Err(e) => {
                            error!("wallet connection failed: {e}");
                            backoff = (backoff.max(1) * 2).min(32);
                            sleep(Duration::from_secs(backoff)).await;
                        }
                    }
                }
            });
        }
    }
}
