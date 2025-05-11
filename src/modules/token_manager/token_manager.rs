use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, error};
use crate::modules::utils::types::EnrichedTradeEvent;

pub struct TokenWorkerManager {
    /// Pour chaque token, un sender asynchrone de trades
    workers: Arc<DashMap<String, mpsc::Sender<EnrichedTradeEvent>>>,
    /// Taille du buffer mpsc par worker
    capacity: usize,
}

impl TokenWorkerManager {
    /// `capacity` = taille du buffer mpsc par token
    pub fn new(capacity: usize) -> Self {
        Self {
            workers: Arc::new(DashMap::new()),
            capacity,
        }
    }

    /// Renvoie (ou crée) le sender pour ce token
    pub fn ensure_worker(&self, token: &str) -> mpsc::Sender<EnrichedTradeEvent> {
        if let Some(tx) = self.workers.get(token) {
            return tx.clone();
        }

        // Création d’un canal mpsc borné
        let (tx, mut rx) = mpsc::channel(self.capacity);
        self.workers.insert(token.to_string(), tx.clone());

        let token_clone = token.to_string();
        // Task Tokio dédiée, traitement séquentiel de rx
        tokio::spawn(async move {
            info!("🆕 Worker démarré pour token {}", token_clone);
            while let Some(trade) = rx.recv().await {
                // TODO: remplacer par votre logique de traitement
                // info!("Trade reçu pour {}: {:?}", token_clone, trade);
            }
            info!("🛑 Worker arrêté pour token {}", token_clone);
        });

        tx
    }

    /// Envoie un EnrichedTradeEvent à son worker.
    /// Si le buffer est plein, `.send().await` retournera Err après avoir attendu.
    pub async fn route_trade(&self, token: &str, trade: EnrichedTradeEvent) {
        let tx = self.ensure_worker(token);
        if let Err(e) = tx.send(trade).await {
            error!("Erreur en envoyant vers worker {}: {:?}", token, e);
            // Si le worker est fermé, on le retire pour recréer plus tard
            self.workers.remove(token);
        }
    }
}
