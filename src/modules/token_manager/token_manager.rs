use std::sync::Arc;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc::{self, channel, Sender, UnboundedReceiver, UnboundedSender};
use tracing::{error, info};

use crate::modules::utils::types::EnrichedTradeEvent;

/*──────────────────── Structures publiques ────────────────────*/

/// Ordre de vente envoyé par un worker → main
#[derive(Clone, Debug)]
pub struct SellOrder {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub creator: Pubkey,
    pub token_amount: u64,   // solde du bot à liquider
}

/*──────────────────── Structures internes ────────────────────*/

/// Méta-données + position courante d’un token
#[derive(Clone, Copy, Debug)]
pub struct TokenMeta {
    pub bonding_curve: Pubkey,
    pub creator: Pubkey,
    pub balance: u64,          // tokens détenus par le bot
}

/// Manager principal
pub struct TokenWorkerManager {
    workers:      Arc<DashMap<String, Sender<EnrichedTradeEvent>>>,
    metas:        Arc<DashMap<String, TokenMeta>>,
    capacity:     usize,
    event_tx:     UnboundedSender<EnrichedTradeEvent>,
    sell_req_tx:  mpsc::Sender<SellOrder>,
}

/*──────────────────── Implémentation ────────────────────*/

impl TokenWorkerManager {
    pub fn new(
        capacity: usize,
        sell_req_tx: mpsc::Sender<SellOrder>,
    ) -> (Self, UnboundedReceiver<EnrichedTradeEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Self {
                workers: Arc::new(DashMap::with_capacity(capacity)),
                metas:   Arc::new(DashMap::with_capacity(capacity)),
                capacity,
                event_tx,
                sell_req_tx,
            },
            event_rx,
        )
    }

    /*───── APIs appelées depuis main.rs ─────*/

    /// Enregistre les infos statiques du CreateEvent
    pub fn register_meta(&self, mint: &Pubkey, bonding_curve: &Pubkey, creator: &Pubkey) {
        self.metas.insert(
            mint.to_string(),
            TokenMeta { bonding_curve: *bonding_curve, creator: *creator, balance: 0 },
        );
    }

    /// Après confirmation du BUY → on connaît la quantité réelle
    pub fn update_balance(&self, mint: &Pubkey, new_balance: u64) {
        if let Some(mut meta) = self.metas.get_mut(&mint.to_string()) {
            meta.balance = new_balance;
        }
    }

    /// Après confirmation du SELL (solde totalement liquidé)
    pub fn deduct_balance(&self, mint: &Pubkey) {
        if let Some(mut meta) = self.metas.get_mut(&mint.to_string()) {
            meta.balance = 0;
        }
    }

    /*───── Worker lifecycle & routage ─────*/

    /// Retourne (ou crée) le Sender associé au token
    pub fn ensure_worker(&self, token: &str) -> Sender<EnrichedTradeEvent> {
        if let Some(tx) = self.workers.get(token) {
            return tx.clone();
        }

        // nouveau canal borné
        let (tx, mut rx) = channel(self.capacity);
        self.workers.insert(token.to_string(), tx.clone());

        // clones pour le task
        let token_string = token.to_string();
        let event_tx     = self.event_tx.clone();
        let sell_tx      = self.sell_req_tx.clone();
        let metas        = Arc::clone(&self.metas);

        tokio::spawn(async move {
            info!("🆕 Worker démarré pour {token_string}");
            let mut entry_mc: Option<f64> = None;
            let take_profit = 10.0;   // +30 %
            let stop_loss   = -2.0;   // –3 %

            while let Some(trade) = rx.recv().await {
                let _ = event_tx.send(trade.clone());   // diffusion globale

                // MC courante
                let mc = crate::modules::monitoring::transaction_verifier::market_cap(
                    trade.trade.virtual_sol_reserves,
                    trade.trade.virtual_token_reserves,
                );

                if entry_mc.is_none() {
                    entry_mc = Some(mc);     // prix d’entrée
                    continue;
                }
                let pct = (mc / entry_mc.unwrap() - 1.0) * 100.0;

                if pct >= take_profit || pct <= stop_loss {
                    // solde détenu ?
                    if let Some(meta) = metas.get(&token_string) {
                        if meta.balance > 0 {
                            let _ = sell_tx.send(SellOrder {
                                mint: trade.trade.mint,
                                bonding_curve: meta.bonding_curve,
                                creator: meta.creator,
                                token_amount: meta.balance,
                            }).await;
                            info!("🎯 Seuil atteint → demande de SELL pour {token_string}");
                        } else {
                            info!("⚠️ Pas de solde à vendre pour {token_string}");
                        }
                    } else {
                        error!("Meta manquante pour {token_string}");
                    }
                    break;  // worker stoppe après la vente
                }
            }
            info!("🛑 Worker arrêté pour {token_string}");
        });

        tx
    }

    /// Route un trade au worker adapté
    pub async fn route_trade(&self, token: &str, trade: EnrichedTradeEvent) {
        let tx = self.ensure_worker(token);
        if let Err(e) = tx.send(trade).await {
            error!("Worker mort pour {token}: {e:?}");
            self.workers.remove(token);
        }
    }
}
