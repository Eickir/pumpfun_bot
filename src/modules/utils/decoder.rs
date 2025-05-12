//! src/modules/utils/decoder.rs  – v2 (optimisée)

use crate::modules::utils::types::{CreateEvent, TradeEvent, ParsedEvent};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use borsh::BorshDeserialize;

use std::{
    cell::RefCell,
    convert::TryInto,
};

/// -------------------------------------------------------------------------
/// 1. Buffer TLS : réutilisé entre appels pour éviter les reallocations
/// -------------------------------------------------------------------------
thread_local! {
    static DECODE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

/// -------------------------------------------------------------------------
/// 2. Discriminants (prefixes) encodés en u64 little-endian
/// -------------------------------------------------------------------------
#[inline(always)]
const fn le_u64(bytes: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*bytes)
}

const CREATE_TAG: u64 = le_u64(b"\x1B\x72\xA9\x4D\xDE\xEB\x63\x76");
const TRADE_TAG : u64 = le_u64(b"\xBD\xDB\x7F\xD3\x4E\xE6\x61\xEE");

/// -------------------------------------------------------------------------
/// 3. Décodage d’un log : 8 octets de discriminant + payload Borsh
/// -------------------------------------------------------------------------
#[inline(always)]
pub fn decode_event(log: &[u8]) -> Result<ParsedEvent, String> {
    if log.len() < 8 {
        return Err("Log trop court".into());
    }

    // lecture little-endian en u64 (1 comparaison au lieu de 8 bytes)
    let tag = {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&log[..8]);
        u64::from_le_bytes(arr)
    };

    let mut payload = &log[8..]; // pointeur sur la partie Borsh

    match tag {
        CREATE_TAG => CreateEvent::deserialize(&mut payload)
            .map(ParsedEvent::Create)
            .map_err(|e| e.to_string()),
        TRADE_TAG  => TradeEvent::deserialize(&mut payload)
            .map(ParsedEvent::Trade)
            .map_err(|e| e.to_string()),
        _ => Err("Discriminant inconnu".into()),
    }
}

/// -------------------------------------------------------------------------
/// 4. Extraction & base64-decode de tous les logs “Program data: …”
///    – réutilise le buffer TLS
///    – base64::decode_slice_unchecked (1 seule passe, sans alloc)
/// -------------------------------------------------------------------------
#[inline(always)]
pub fn extract_program_logs(tx: &SubscribeUpdateTransaction) -> Vec<Vec<u8>> {
    let mut out = Vec::new();

    // Fast-path : on s’arrête si pas de meta
    let meta = match tx.transaction.as_ref().and_then(|t| t.meta.as_ref()) {
        Some(m) => m,
        None => return out,
    };
    out.reserve(meta.log_messages.len());

    for log in &meta.log_messages {
        let raw = log.as_bytes();
        if !raw.starts_with(b"Program data: ") {
            continue;
        }

        // après le tag ASCII "Program data: " (14 octets)
        let enc = &raw[14..];

        DECODE_BUF.with(|tls| {
            let mut buf = tls.borrow_mut();

            // capacité nécessaire ≈ (n * 3 / 4), arrondi au supérieur
            let needed = (enc.len() * 3 + 3) / 4;
            buf.resize(needed, 0);             // initialise à zéro (stable)

            // 1 passe base64 « unchecked » (logs Pump.fun ⇒ réputés valides)
            // SAFETY: enc est du base64 correct
            let len = unsafe {
                STANDARD
                    .decode_slice_unchecked(enc, &mut buf[..])
                    .expect("base64 decode failed")
            };
            buf.truncate(len);

            out.push(buf.clone()); // clone car TLS sera réutilisé
        });
    }

    out
}
