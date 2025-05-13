use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use dashmap::DashMap;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;
use solana_sdk::{signature::Signature, pubkey::Pubkey};

use crate::modules::utils::{
    decoder::{extract_program_logs, decode_event},
    types::{ParsedEvent},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxStatus { Pending, Successed, Failed }

#[derive(Clone, Debug)]
pub struct ConfirmationState {
    pub token_pubkey: Pubkey,
    pub bonding_curve: Pubkey,
    pub token_amount: u64,
    pub sol_amount: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub notifier: watch::Sender<TxStatus>,
}

pub type PendingTxMap = Arc<DashMap<Signature, ConfirmationState>>;

/// unique consumer: on reçoit directement le mpsc::Receiver
pub async fn confirm_wallet_transaction(
    mut wallet_rx: mpsc::Receiver<Arc<SubscribeUpdateTransaction>>,
    pending: PendingTxMap,
    wallet_balance: Arc<AtomicU64>,
) {
    while let Some(wtx_arc) = wallet_rx.recv().await {
        let wtx = &*wtx_arc;

        let sig = match wtx.transaction
            .as_ref()
            .and_then(|t| Signature::try_from(t.signature.clone()).ok())
        {
            Some(s) => s,
            None => continue,
        };

        let mut conf = match pending.get_mut(&sig) {
            Some(c) => c,
            None => continue,
        };

        let Some(meta) = wtx.transaction.as_ref().and_then(|t| t.meta.as_ref()) else {
            pending.remove(&sig);
            continue;
        };

        wallet_balance.store(meta.post_balances[0], Ordering::Relaxed);

        let status = if meta.err.is_none() { TxStatus::Successed } else { TxStatus::Failed };
        let _ = conf.notifier.send(status.clone());

        if status == TxStatus::Successed {
            for raw in extract_program_logs(wtx) {
                if let Ok(ParsedEvent::Trade(t)) = decode_event(&raw) {
                    conf.token_amount             = t.token_amount;
                    conf.virtual_sol_reserves     = t.virtual_sol_reserves;
                    conf.virtual_token_reserves   = t.virtual_token_reserves;
                    conf.sol_amount               = t.sol_amount;
                }
            }
            info!("✅ tx {sig} confirmé — token {}", conf.token_pubkey);
        } else {
            error!("❌ tx {sig} failed");
            pending.remove(&sig);
        }
    }
}


/// Calcule la market-cap en SOL (f64)
#[inline]
pub fn market_cap(v_sol: u64, v_tok: u64) -> f64 {
    if v_tok == 0 {
        return 0.0;                     // évite la division par zéro
    }
    let sol = v_sol as f64 / 1_000_000_000.0;   // virtual SOL réservés
    let tok = v_tok as f64 / 1_000_000.0;       // virtual TOKEN en circulation

    (sol / tok) * 1_000_000_000.0
}
