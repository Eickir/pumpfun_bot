//! modules/token_manager/token_manager.rs
//! v2 – ajoute la diffusion des trades aux observateurs externes

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::mpsc::{
    channel, unbounded_channel, Sender, UnboundedReceiver, UnboundedSender,
};
use tracing::{error, info};

use crate::modules::utils::types::EnrichedTradeEvent;

/// Gère un pool de workers (un par token) et permet d’observer
/// tous les trades traités via un canal global `event_rx`.
pub struct TokenWorkerManager {
    /// Pour chaque token, le `Sender` asynchrone relié au worker
    workers: Arc<DashMap<String, Sender<EnrichedTradeEvent>>>,
    /// Taille du buffer MPSC de chaque worker
    capacity: usize,
    /// Canal global : chaque trade routé est également envoyé ici
    event_tx: UnboundedSender<EnrichedTradeEvent>,
}

impl TokenWorkerManager {
    /// Construit le manager.
    ///
    /// * `capacity` – taille du buffer (nombre de trades en attente) de chaque worker.
    ///
    /// Retourne `(manager, event_rx)` :  
    /// le `event_rx` permet à l’appelant d’observer tous les trades entrants.
    pub fn new(
        capacity: usize,
    ) -> (Self, UnboundedReceiver<EnrichedTradeEvent>) {
        let (event_tx, event_rx) = unbounded_channel();
        (
            Self {
                workers: Arc::new(DashMap::with_capacity(capacity)),
                capacity,
                event_tx,
            },
            event_rx,
        )
    }

    /// Retourne (ou crée) le `Sender` associé au token.
    pub fn ensure_worker(&self, token: &str) -> Sender<EnrichedTradeEvent> {
        if let Some(tx) = self.workers.get(token) {
            return tx.clone();
        }

        // nouveau canal borné pour ce token
        let (tx, mut rx) = channel(self.capacity);
        self.workers.insert(token.to_string(), tx.clone());

        // lancement du worker asynchrone
        let token_string = token.to_string();
        tokio::spawn(async move {
            info!("🆕 Worker démarré pour token {token_string}");
            while let Some(trade) = rx.recv().await {
                // TODO : logique métier sur chaque trade
                // ex. analyse, update de state, arbitrage, etc.
                // info!("Trade {token_string} reçu : {:?}", trade);
            }
            info!("🛑 Worker arrêté pour token {token_string}");
        });

        tx
    }

    /// Envoie un trade au worker dédié **et** le publie sur le bus global.
    pub async fn route_trade(&self, token: &str, trade: EnrichedTradeEvent) {
        // 1) diffusion externe (non bloquante)
        let _ = self.event_tx.send(trade.clone());

        // 2) routage vers le worker
        let tx = self.ensure_worker(token);
        if let Err(e) = tx.send(trade).await {
            error!("Erreur d’envoi vers worker {token}: {e:?}");
            // worker mort ? ⇒ on le retire pour recréation ultérieure
            self.workers.remove(token);
        }
    }
}
