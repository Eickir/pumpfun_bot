use solana_client::nonblocking::rpc_client::RpcClient;
use crate::modules::utils::types::TradeEvent;
use crate::modules::wallet::constants::{TOKEN_PROGRAM_ID, SPL_TOKEN_PROGRAM_ID};
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use std::sync::Arc;
use solana_sdk::commitment_config::CommitmentConfig;
use std::error::Error;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::transaction::VersionedTransaction;
use solana_program::instruction::Instruction;
use solana_sdk::hash::Hash;
use spl_associated_token_account::instruction::create_associated_token_account;
use spl_associated_token_account::get_associated_token_address;
use crate::modules::wallet::utils::buy_instructions_arguments;
use crate::modules::wallet::utils::buy_instructions;
use crate::modules::wallet::utils::sell_instructions;
use crate::modules::wallet::instructions::Buy;
use crate::modules::wallet::instructions::Sell;
use spl_token::instruction::close_account;
use solana_program::message::VersionedMessage;
use solana_program::message::v0;

/// The main Wallet struct. Note how keypair is wrapped in `Arc<Keypair>`.
#[derive(Clone)]
pub struct Wallet {
    /// Use Arc so we can safely clone references without copying secret key data.
    pub keypair: Arc<Keypair>,
    pub pubkey: Pubkey,
    pub rpc_client: Arc<RpcClient>,
}

impl Wallet {
    /// Construct a new Wallet, storing the keypair in an Arc.
    pub fn new(keypair: Keypair, rpc_url: &str) -> Self {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::processed(),
        ));
        let pubkey = keypair.pubkey();

        Self {
            keypair:Arc::new(keypair),
            pubkey,
            rpc_client
        }
    }

    /// Example function to fetch the wallet’s SOL balance.
    pub async fn get_balance(&self) -> Result<u64, Box<dyn Error + Send + Sync>> {
        let balance = self.rpc_client.get_balance(&self.pubkey).await?;
        Ok(balance)
    }

    pub async fn buy_transaction(
        &self,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        creator: &Pubkey,
        sol_amount: f64,
        slippage: f64,
        dev_trade: TradeEvent,
        blockhash: Hash,
    ) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {
    
        let mut instructions: Vec<Instruction> = vec![];
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(100_000));
    
        instructions.push(create_associated_token_account(
            &self.pubkey,
            &self.pubkey,
            &mint,
            &TOKEN_PROGRAM_ID,
        ));
    
        let (token_amount, max_sol_cost) = buy_instructions_arguments(
            dev_trade.virtual_sol_reserves, 
            dev_trade.virtual_token_reserves, 
            sol_amount, 
            slippage,
        );
    
        instructions.push(buy_instructions(
            &self.keypair, 
            &mint, 
            &bonding_curve, 
            &creator, 
            Buy { _amount: token_amount, _max_sol_cost: max_sol_cost }
        ));
    
        let v0_message = v0::Message::try_compile(&self.pubkey, &instructions, &[], blockhash)?;
        let versioned_message = VersionedMessage::V0(v0_message);
        let transaction = VersionedTransaction::try_new(versioned_message, &[&self.keypair])?;
    
        Ok(transaction)
    }
    


    pub async fn sell_transaction(
        &self,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        creator: &Pubkey,
        token_amount: u64, 
        blockhash: Hash,
    ) -> Result<VersionedTransaction, Box<dyn Error + Send + Sync>> {

        let mut instructions: Vec<Instruction> = vec![];

        // priority fee 
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(100_000));

        // sell instructions 
        instructions.push(sell_instructions(&self.keypair, &mint, &bonding_curve, &creator, Sell {_amount: token_amount,   _min_sol_output: 0}));

        // close associated token account
        let associated_user = get_associated_token_address(&self.pubkey, &mint);
        instructions.push(close_account(
            &SPL_TOKEN_PROGRAM_ID,
            &associated_user,
            &self.pubkey,
            &self.pubkey,
            &[&self.pubkey],
        )?);

        // build transaction 
        let v0_message= v0::Message::try_compile(&self.pubkey, &instructions, &[], blockhash)?;
        let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);
        let transaction = VersionedTransaction::try_new(versioned_message, &[&self.keypair])?;

        Ok(transaction)
    }


}