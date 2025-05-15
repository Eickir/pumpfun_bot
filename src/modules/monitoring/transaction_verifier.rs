use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use bincode::deserialize;
use dashmap::DashMap;
use solana_sdk::{
    instruction::InstructionError,
    transaction::TransactionError,
    pubkey::Pubkey,
    signature::Signature,
};
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;

use crate::modules::utils::{
    decoder::{decode_event, extract_program_logs},
    types::ParsedEvent,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Pending,
    Successed,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ConfirmationState {
    pub token_pubkey:           Pubkey,
    pub bonding_curve:          Pubkey,
    pub token_amount:           u64,
    pub sol_amount:             u64,
    pub virtual_sol_reserves:   u64,
    pub virtual_token_reserves: u64,
    pub notifier:               watch::Sender<TxStatus>,
}

pub type PendingTxMap = Arc<DashMap<Signature, ConfirmationState>>;

/// Désérialise les bytes d’erreur bincode et
/// renvoie `Some(code)` si c’est un `InstructionError::Custom(code)`.
#[inline]
fn extract_error_code(err_bytes: &[u8]) -> Option<u32> {
    let tx_err: TransactionError = deserialize(err_bytes).ok()?;
    match tx_err {
        TransactionError::InstructionError(_ix, InstructionError::Custom(code)) => Some(code),
        _ => None,
    }
}

/// Consomme le flux wallet, met à jour `pending` et `wallet_balance`.
pub async fn confirm_wallet_transaction(
    mut wallet_rx: mpsc::Receiver<Arc<SubscribeUpdateTransaction>>,
    pending:      PendingTxMap,
    wallet_balance: Arc<AtomicU64>,
) {
    while let Some(wtx_arc) = wallet_rx.recv().await {
        let wtx = &*wtx_arc;

        // 1. Extraire la signature
        let sig = match wtx.transaction
            .as_ref()
            .and_then(|t| Signature::try_from(t.signature.clone()).ok())
        {
            Some(s) => s,
            None    => continue,
        };

        // 2. Récupérer l’état en attente
        let mut conf = match pending.get_mut(&sig) {
            Some(c) => c,
            None    => continue,
        };

        // 3. Récupérer meta (post_balances + err)
        let Some(meta) = wtx.transaction
            .as_ref()
            .and_then(|t| t.meta.as_ref())
        else {
            // pas de meta ⇒ abandon
            pending.remove(&sig);
            continue;
        };

        // 4. Met à jour le solde on‐chain
        wallet_balance.store(meta.post_balances[0], Ordering::Relaxed);

        // 5. Déterminer le statut en tenant compte du code 3012
        let status = if meta.err.is_none() {
            TxStatus::Successed
        } else {
            // meta.err contient les bytes bincode de TransactionError
            let err_bytes = &meta.err.as_ref().unwrap().err;
            if let Some(3012) = extract_error_code(&err_bytes) {
                TxStatus::Successed
            } else {
                TxStatus::Failed
            }
        };

        // 6. Notifier le watcher interne
        let _ = conf.notifier.send(status.clone());

        // 7. Brancher selon succès ou échec
        if status == TxStatus::Successed {
            // mettre à jour les champs depuis les logs
            for raw in extract_program_logs(wtx) {
                if let Ok(ParsedEvent::Trade(t)) = decode_event(&raw) {
                    conf.token_amount           = t.token_amount;
                    conf.virtual_sol_reserves   = t.virtual_sol_reserves;
                    conf.virtual_token_reserves = t.virtual_token_reserves;
                    conf.sol_amount             = t.sol_amount;
                }
            }
            info!(
                "✅ tx {} confirmé — token {}",
                sig, conf.token_pubkey
            );
            // on garde l’entrée dans pending si besoin de relecture
        } else {
            error!("❌ tx {} failed", sig);
            // abandon définitif
            pending.remove(&sig);
        }
    }
}

/// Calcule la market-cap en SOL (f64)
#[inline]
pub fn market_cap(v_sol: u64, v_tok: u64) -> f64 {
    if v_tok == 0 {
        return 0.0;
    }
    let sol = v_sol as f64 / 1_000_000_000.0;
    let tok = v_tok as f64 / 1_000_000.0;
    (sol / tok) * 1_000_000_000.0
}
