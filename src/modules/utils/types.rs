use solana_sdk::pubkey::Pubkey;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::Serialize;
use solana_sdk::signature::Signature;
use clickhouse::Row;

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct TradeEvent {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
    pub timestamp: i64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
}

#[derive(Debug, Clone)]
pub struct EnrichedTradeEvent {
    pub trade: TradeEvent,
    pub tx_id: Signature, 
    pub slot: u64,
    pub tx_index: u64
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct CreateEvent {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub user: Pubkey,
}

#[derive(Debug, Clone)]
pub enum ParsedEvent {
    Create(CreateEvent),
    Trade(TradeEvent),
}

#[derive(Debug, Serialize, Row)]
pub struct TradeEventRecord {
    pub tx_id: String, 
    pub mint: String,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: String,
    pub timestamp: i64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub tx_slot: u64,
    pub tx_index: u64
}

impl From<EnrichedTradeEvent> for TradeEventRecord {
    fn from(e: EnrichedTradeEvent) -> Self {
        Self {
            tx_id: e.tx_id.to_string(), 
            mint: e.trade.mint.to_string(),
            sol_amount: e.trade.sol_amount,
            token_amount: e.trade.token_amount,
            is_buy: e.trade.is_buy,
            user: e.trade.user.to_string(),
            timestamp: e.trade.timestamp,
            virtual_sol_reserves: e.trade.virtual_sol_reserves,
            virtual_token_reserves: e.trade.virtual_token_reserves,
            real_sol_reserves: e.trade.real_sol_reserves,
            real_token_reserves: e.trade.real_token_reserves,
            tx_slot: e.slot,
            tx_index: e.tx_index
        }
    }
}

#[derive(Row, Clone, Debug, Serialize)]
pub struct CreatedTokenRecord {
    pub mint: String,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub creator: String,
    pub tx_slot: u64,
    pub tx_index: u64,
    pub tx_id: String,
}