// src/modules/utils/decoder.rs

use crate::modules::utils::types::{CreateEvent, TradeEvent, ParsedEvent};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use borsh::BorshDeserialize;
use std::{cell::RefCell, convert::TryInto, mem};
use tracing::debug;

// Buffer TLS pour éviter les allocations répétées
thread_local! {
    static DECODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

// Préfixes “magic” byte-à-byte
const CREATE_PREFIX: [u8; 8] = [27, 114, 169, 77, 222, 235, 99, 118];
const TRADE_PREFIX:  [u8; 8] = [189, 219, 127, 211,  78, 230,  97, 238];

/// Tente de désérialiser un log Borsh+Anchor (8-byte prefix + struct).
/// Utilise `deserialize(&mut &[u8])` pour ignorer les octets restants.
pub fn decode_event(log: &[u8]) -> Result<ParsedEvent, String> {
    if log.len() < 8 {
        return Err("Log trop court".into());
    }
    // Récupère les 8 premiers octets tels quels
    let prefix: [u8; 8] = log[0..8].try_into().unwrap();
    // Le reste est le payload Borsh
    let mut data = &log[8..];

    debug!("decode_event prefix = {:02x?}", prefix);

    if prefix == CREATE_PREFIX {
        // Deserialize consommera seulement ce qu'il faut
        CreateEvent::deserialize(&mut data)
            .map(ParsedEvent::Create)
            .map_err(|e| e.to_string())
    } else if prefix == TRADE_PREFIX {
        TradeEvent::deserialize(&mut data)
            .map(ParsedEvent::Trade)
            .map_err(|e| e.to_string())
    } else {
        Err(format!("Discriminant inconnu : {:02x?}", prefix))
    }
}

/// Extrait et décode tous les logs “Program data: …” d’une transaction gRPC,
/// en réutilisant un buffer TLS et sans clone superflu.
pub fn extract_program_logs(tx: &SubscribeUpdateTransaction) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(tx_data) = &tx.transaction {
        if let Some(meta) = &tx_data.meta {
            out.reserve(meta.log_messages.len());
            for log in &meta.log_messages {
                if log.as_bytes().starts_with(b"Program data: ") {
                    let enc = &log.as_bytes()[14..];
                    DECODE_BUF.with(|c| {
                        let mut buf = c.borrow_mut();
                        buf.clear();
                        if STANDARD.decode_vec(enc, &mut buf).is_ok() {
                            let mut extracted = Vec::new();
                            mem::swap(&mut *buf, &mut extracted);
                            out.push(extracted);
                        }
                    });
                }
            }
        }
    }
    out
}
