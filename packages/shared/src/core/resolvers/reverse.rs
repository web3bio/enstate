use ethers::prelude::{Address, ProviderError};
use ethers_core::abi;
use ethers_core::abi::{ParamType, Token};
use ethers_core::types::transaction::eip2718::TypedTransaction;
use ethers_core::types::{Bytes, U256};
use hex_literal::hex;
use thiserror::Error;
use tracing::instrument;

use crate::core::CCIPProvider;

#[derive(Error, Debug)]
pub enum ReverseResolveError {
    #[error("Address doesn't have a primary name")]
    MissingPrimaryName,

    #[error("Address on name doesn't match with lookup address")]
    AddressMismatch,

    #[error("Failed to lookup address for reverse record: {0}")]
    AddressLookupError(String),

    #[error("RPC provider error: {0}")]
    RPCError(#[from] ProviderError),

    #[error("ABI decode error: {0}")]
    AbiDecodeError(#[from] abi::Error),
}

const REVERSE_SELECTOR: [u8; 4] = hex!("5d78a217");
const ETH_COIN_TYPE: u64 = 60;

#[instrument(skip(rpc))]
pub async fn resolve_reverse(
    rpc: &CCIPProvider,
    address: &Address,
    universal_resolver: &Address,
) -> Result<String, ReverseResolveError> {
    resolve_reverse_coin_type(rpc, address, ETH_COIN_TYPE, universal_resolver).await
}

#[instrument(skip(rpc))]
pub async fn resolve_reverse_coin_type(
    rpc: &CCIPProvider,
    address: &Address,
    coin_type: u64,
    universal_resolver: &Address,
) -> Result<String, ReverseResolveError> {
    let mut transaction = TypedTransaction::default();

    transaction.set_to(*universal_resolver);

    let encoded = abi::encode(&[
        Token::Bytes(address.as_bytes().to_vec()),
        Token::Uint(U256::from(coin_type)),
    ]);
    transaction.set_data(Bytes::from(
        [&REVERSE_SELECTOR, encoded.as_slice()].concat(),
    ));

    let res = rpc
        .call_ccip(&transaction, None)
        .await
        .map_err(|err| ReverseResolveError::AddressLookupError(err.to_string()))?
        .0;

    let mut decoded = abi::decode(
        &[ParamType::String, ParamType::Address, ParamType::Address],
        &res,
    )?;

    let name = decoded
        .remove(0)
        .into_string()
        .ok_or(ReverseResolveError::AbiDecodeError(abi::Error::InvalidData))?;

    if name.is_empty() {
        return Err(ReverseResolveError::MissingPrimaryName);
    }

    let resolver = decoded
        .remove(0)
        .into_address()
        .ok_or(ReverseResolveError::AbiDecodeError(abi::Error::InvalidData))?;

    if resolver.is_zero() {
        return Err(ReverseResolveError::AddressMismatch);
    }

    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ethers::prelude::{Http, Provider};
    use ethers_ccip_read::CCIPReadMiddleware;

    use crate::core::resolvers::reverse::resolve_reverse;

    #[tokio::test]
    async fn test() {
        let rpc_url = std::env::var("RPC_URL")
            .unwrap_or_else(|_| "https://ethereum.publicnode.com".to_string());
        let provider = Provider::<Http>::try_from(rpc_url).unwrap();

        let provider = CCIPReadMiddleware::new(Arc::new(provider));

        assert_eq!(
            resolve_reverse(
                &provider,
                &"0xb8c2C29ee19D8307cb7255e1Cd9CbDE883A267d5"
                    .parse()
                    .unwrap(),
                &"0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe"
                    .parse()
                    .unwrap(),
            )
            .await
            .ok(),
            Some("nick.eth".to_string())
        );

        assert_eq!(
            resolve_reverse(
                &provider,
                &"0x2B5c7025998f88550Ef2fEce8bf87935f542C190"
                    .parse()
                    .unwrap(),
                &"0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe"
                    .parse()
                    .unwrap(),
            )
            .await
            .ok(),
            Some("antony.sh".to_string())
        );
    }
}
