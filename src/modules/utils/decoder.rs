// src/modules/utils/decoder.rs

use crate::modules::utils::types::{CreateEvent, TradeEvent, ParsedEvent};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use borsh::BorshDeserialize;
use std::{cell::RefCell, convert::TryInto, mem};

// Buffer TLS pour éviter les allocations répétées lors du Base64
thread_local! {
    static DECODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

// Discriminants en u64 pour comparaison en un seul accès
const CREATE_DISCRIMINATOR: u64 = u64::from_le_bytes([27,114,169,77,222,235,99,118]);
const TRADE_DISCRIMINATOR:  u64 = u64::from_le_bytes([189,219,127,211,78,230,97,238]);

/// Décodage d’un événement Borsh à partir d’un log raw
#[inline(always)]
pub fn decode_event(log: &[u8]) -> Result<ParsedEvent, String> {
    if log.len() < 8 {
        return Err("Log trop court".into());
    }
    // Discriminant en un seul u64
    let disc = u64::from_le_bytes(log[0..8].try_into().unwrap());
    let data = &log[8..];

    if disc == CREATE_DISCRIMINATOR {
        // Lecture inline des longueurs
        let mut off = 0;
        let nl = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
        off += 4 + nl;
        let sl = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
        off += 4 + sl;
        let ul = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
        off += 4 + ul;
        // trois champs fixes
        off += 32 + 32 + 32;

        let slice = data.get(..off).ok_or("Slice hors limites")?;
        CreateEvent::try_from_slice(slice)
            .map(ParsedEvent::Create)
            .map_err(|e| e.to_string())
    } else if disc == TRADE_DISCRIMINATOR {
        TradeEvent::try_from_slice(data)
            .map(ParsedEvent::Trade)
            .map_err(|e| e.to_string())
    } else {
        Err("Type d’événement inconnu".into())
    }
}

/// Extrait et décode tous les logs "Program data: ..." d’une TX gRPC,
/// en réutilisant un buffer TLS et sans faire de clones superflus.
pub fn extract_program_logs(tx: &SubscribeUpdateTransaction) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(tx_data) = &tx.transaction {
        if let Some(meta) = &tx_data.meta {
            out.reserve(meta.log_messages.len());
            for log in &meta.log_messages {
                // détection manuelle du préfixe (un seul starts_with)
                if log.len() > 14 && log.as_bytes().starts_with(b"Program data: ") {
                    let enc = &log[14..];
                    DECODE_BUF.with(|c| {
                        let mut buf = c.borrow_mut();
                        buf.clear();
                        // décodage direct dans buf (évite alloc intermédiaire)
                        if STANDARD.decode_vec(enc, &mut buf).is_ok() {
                            // swap O(1) pour récupérer buf complet
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
