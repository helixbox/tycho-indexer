use core::fmt::Debug;
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

use crate::{
    models::{
        blockchain::{Block, BlockTag},
        contract::AccountDelta,
        token::{CurrencyToken, TokenQuality, TransferCost, TransferTax},
        Address, Balance,
    },
    Bytes,
};

#[async_trait]
pub trait AccountExtractor {
    type Error;

    async fn get_accounts(
        &self,
        block: Block,
        account_addresses: Vec<Address>,
    ) -> Result<HashMap<Bytes, AccountDelta>, Self::Error>; //TODO: do not return `AccountUpdate` but `Account`
}

/// Trait for analyzing a token, including its quality, transfer cost, and transfer tax.
#[async_trait]
pub trait TokenAnalyzer: Send + Sync {
    type Error;

    /// Analyzes the quality of a token given its address and a block tag.
    ///
    /// # Parameters
    /// * `token` - The address of the token to analyze.
    /// * `block` - The block tag at which the analysis should be performed.
    ///
    /// # Returns
    /// A result containing:
    /// * `TokenQuality` - The quality assessment of the token (either `Good` or `Bad`).
    /// * `Option<TransferCost>` - The average cost per transfer, if available.
    /// * `Option<TransferTax>` - The transfer tax, if applicable.
    ///
    /// On failure, returns `Self::Error`.
    async fn analyze(
        &self,
        token: Bytes,
        block: BlockTag,
    ) -> Result<(TokenQuality, Option<TransferCost>, Option<TransferTax>), Self::Error>;
}

/// Trait for finding an address that owns a specific token. This is useful for detecting
/// bad tokens by identifying addresses with enough balance to simulate transactions.
#[async_trait]
pub trait TokenOwnerFinding: Send + Sync + Debug {
    /// Finds an address that holds at least `min_balance` of the specified token.
    ///
    /// # Parameters
    /// * `token` - The address of the token to search for.
    /// * `min_balance` - The minimum balance required for the address to be considered.
    ///
    /// # Returns
    /// A result containing:
    /// * `Option<(Address, Balance)>` - The address and its actual balance if an owner is found.
    /// If no address meets the criteria, returns `None`.
    /// On failure, returns a string representing an error message.
    async fn find_owner(
        &self,
        token: Address,
        min_balance: Balance,
    ) -> Result<Option<(Address, Balance)>, String>; // TODO: introduce custom error type
}

/// Trait for retrieving additional information about tokens, such as the number of decimals
/// and the token symbol, to help construct `CurrencyToken` objects.
#[async_trait]
pub trait TokenPreProcessor: Send + Sync {
    /// Given a list of token addresses, this function retrieves additional metadata for each token.
    ///
    /// # Parameters
    /// * `addresses` - A vector of token addresses to process.
    /// * `token_finder` - A reference to a `TokenOwnerFinding` implementation to help find token
    ///   owners.
    /// * `block` - The block tag at which the information should be retrieved.
    ///
    /// # Returns
    /// A vector of `CurrencyToken` objects, each containing the processed information for the
    /// token.
    async fn get_tokens(
        &self,
        addresses: Vec<Bytes>,
        token_finder: Arc<dyn TokenOwnerFinding>,
        block: BlockTag,
    ) -> Vec<CurrencyToken>;
}
