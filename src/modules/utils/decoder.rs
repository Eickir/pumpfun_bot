use crate::modules::utils::types::{CreateEvent, TradeEvent, ParsedEvent};
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction;
use base64::engine::general_purpose::STANDARD;
use borsh::BorshDeserialize;
use base64::Engine;


const TRADE_EVENT_DISCRIMINATOR: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];
const CREATE_EVENT_DISCRIMINATOR: [u8; 8] = [27, 114, 169, 77, 222, 235, 99, 118];

pub fn decode_event(log: &[u8]) -> Result<ParsedEvent, String> {
    if log.len() < 8 {
        return Err("Log too short to contain discriminator".into());
    }

    let discriminator = &log[..8];

    if discriminator == CREATE_EVENT_DISCRIMINATOR {
        let data = &log[8..];
        let mut offset = 0;

        let name_len = get_u32(&data[offset..])?;
        offset += 4 + name_len;

        let symbol_len = get_u32(&data[offset..])?;
        offset += 4 + symbol_len;

        let uri_len = get_u32(&data[offset..])?;
        offset += 4 + uri_len;

        offset += 32 + 32 + 32;

        let slice = data.get(..offset).ok_or("Slice out of bounds")?;
        CreateEvent::try_from_slice(slice)
            .map(ParsedEvent::Create)
            .map_err(|e| e.to_string())
    } else if discriminator == TRADE_EVENT_DISCRIMINATOR {
        TradeEvent::try_from_slice(&log[8..])
            .map(ParsedEvent::Trade)
            .map_err(|e| e.to_string())
    } else {
        Err("Unknown event type".into())
    }
}

fn get_u32(slice: &[u8]) -> Result<usize, String> {
    if slice.len() < 4 {
        return Err("Slice too short for u32".into());
    }
    Ok(u32::from_le_bytes(slice[..4].try_into().unwrap()) as usize)
}

pub fn extract_program_logs(tx: &SubscribeUpdateTransaction) -> Vec<Vec<u8>> {
    if let Some(tx_data) = tx.transaction.as_ref() {
        if let Some(meta) = tx_data.meta.as_ref() {
            return meta.log_messages.iter()
                .filter_map(|log| {
                    log.strip_prefix("Program data: ")
                        .and_then(|data| STANDARD.decode(data).ok())
                })
                .collect();
        }
    }
    Vec::new()
}
