use std::vec;

use ethers::prelude::ProviderError::JsonRpcClientError;
use ethers::{
    providers::namehash,
    types::{transaction::eip2718::TypedTransaction, Address, Bytes},
};
use ethers_ccip_read::{CCIPReadMiddlewareError, CCIPRequest};
use ethers_core::abi;
use ethers_core::abi::{ParamType, Token};
use ethers_core::types::H160;
use hex_literal::hex;
use lazy_static::lazy_static;
use tracing::{instrument, span};

use crate::core::error::ProfileError;
use crate::core::CCIPProvider;
use crate::models::lookup::ENSLookup;
use crate::utils::dns::dns_encode;
use crate::utils::vec::dedup_ord;

lazy_static! {
    static ref OFFCHAIN_DNS_RESOLVER: Address =
        Address::from(hex!("F142B308cF687d4358410a4cB885513b30A42025"));
}

#[derive(Debug, Clone)]
pub struct UniversalResolverResult {
    pub(crate) success: bool,
    pub(crate) data: Vec<u8>,
}

#[instrument(skip(provider))]
pub async fn resolve_universal(
    name: &str,
    data: &[ENSLookup],
    provider: &CCIPProvider,
    universal_resolver: &H160,
) -> Result<(Vec<UniversalResolverResult>, Address, Vec<String>), ProfileError> {
    let name_hash = namehash(name);

    // Prepare the variables
    let dns_encoded_node = dns_encode(name).map_err(ProfileError::DNSEncodeError)?;

    // FIXME: don't force address lookup
    let data = [&[ENSLookup::Addr] as &[ENSLookup], data].concat();

    let multicall_data = data
        .iter()
        .map(|it| it.calldata(&name_hash))
        .map(Token::Bytes)
        .collect();

    // multicall(bytes[] data)
    let multicall_selector = hex_literal::hex!("ac9650d8").to_vec();
    let multicall_payload = [
        multicall_selector,
        abi::encode(&[Token::Array(multicall_data)]),
    ]
    .concat();

    let encoded_data = abi::encode(&[
        Token::Bytes(dns_encoded_node),
        Token::Bytes(multicall_payload),
    ]);

    // resolve(bytes name, bytes data)
    let resolve_selector = hex_literal::hex!("9061b923").to_vec();

    // Create the transaction
    let mut typed_transaction = TypedTransaction::default();

    // Prepare transaction data
    let transaction_data = [resolve_selector, encoded_data].concat();

    // Set up the transaction
    typed_transaction.set_to(*universal_resolver);
    typed_transaction.set_data(Bytes::from(transaction_data));

    let span = span!(tracing::Level::INFO, "ccip_call", name = name);

    // Call the transaction
    let (res, ccip_requests) =
        provider
            .call_ccip(&typed_transaction, None)
            .await
            .map_err(|err| {
                let CCIPReadMiddlewareError::MiddlewareError(provider_error) = err else {
                    return ProfileError::CCIPError(err);
                };

                let JsonRpcClientError(rpc_err) = &provider_error else {
                    return ProfileError::RPCError(provider_error);
                };

                // TODO: better error handling
                if rpc_err.as_error_response().is_some() {
                    return ProfileError::NotFound;
                }

                ProfileError::RPCError(provider_error)
            })?;

    drop(span);

    // Abi Decode
    let result = abi::decode(&[ParamType::Bytes, ParamType::Address], res.as_ref())
        .map_err(|_| ProfileError::ImplementationError("ABI decode failed".to_string()))?;

    if result.len() < 2 {
        // should never trigger
        return Err(ProfileError::ImplementationError("".to_string()));
    }

    let result_data = result.first().expect("result[0] should exist").clone();
    let result_address = result.get(1).expect("result[1] should exist").clone();

    let resolver = result_address
        .into_address()
        .expect("result[1] should be an address");

    let result_data = result_data.into_bytes().expect("result[0] should be bytes");

    let mut result_data = abi::decode(
        &[ParamType::Array(Box::new(ParamType::Bytes))],
        &result_data,
    )
    .map_err(|_| ProfileError::ImplementationError("multicall ABI decode failed".to_string()))?;

    let mut parsed: Vec<UniversalResolverResult> = result_data
        .remove(0)
        .into_array()
        .expect("result[0] should be an array")
        .into_iter()
        .map(|t| {
            let data = t
                .clone()
                .into_bytes()
                .expect("result[0] elements should be bytes");

            let success = data.len() % 32 == 0;

            UniversalResolverResult { success, data }
        })
        .collect();

    let addr = parsed.remove(0);

    // if we got a CCIP response, where the resolver is an OffchainDNSResolver and
    //  the address has failed to resolve, it's a non-existing name
    if resolver.is_zero() || (resolver == *OFFCHAIN_DNS_RESOLVER && !addr.success) {
        return Err(ProfileError::NotFound);
    }

    Ok((
        parsed,
        resolver,
        dedup_ord(
            &ccip_requests
                .iter()
                .flat_map(urls_from_request)
                .collect::<Vec<_>>(),
        ),
    ))
}

fn urls_from_request(request: &CCIPRequest) -> Vec<String> {
    if request.calldata.len() < 4 {
        return Vec::new();
    }

    let decoded = abi::decode(
        &[ParamType::Array(Box::new(ParamType::Tuple(vec![
            ParamType::Address,
            ParamType::Array(Box::new(ParamType::String)),
            ParamType::Bytes,
        ])))],
        &request.calldata[4..],
    )
    .unwrap_or_default();

    let Some(Token::Array(requests)) = decoded.first() else {
        return Vec::new();
    };

    requests
        .iter()
        .flat_map(|request| {
            let Token::Tuple(request) = request else {
                return Vec::new();
            };

            let Some(Token::Array(urls)) = request.get(1) else {
                return Vec::new();
            };

            urls.iter()
                .filter_map(|url| match url {
                    Token::String(url) => Some(url.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use ethers::providers::{Http, Provider};
    use ethers_ccip_read::CCIPReadMiddleware;
    use ethers_core::abi::ParamType;
    use ethers_core::types::Address;
    use serde_json::json;

    use crate::cache::PassthroughCacheLayer;
    use crate::core::lookup_data::LookupInfo;
    use crate::core::resolvers::reverse::{resolve_reverse, resolve_reverse_coin_type};
    use crate::core::resolvers::universal;
    use crate::core::ENSService;
    use crate::models::lookup::{ENSLookup, LookupState};
    use crate::models::multicoin::cointype::coins::CoinType;
    use crate::models::multicoin::cointype::Coins;
    use crate::models::records::Records;
    use crate::utils::factory::SimpleFactory;

    const UNIVERSAL_RESOLVER: &str = "0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe";
    const BASE_COIN_TYPE: u64 = 2147492101;

    fn universal_resolver_address() -> Address {
        Address::from_str(UNIVERSAL_RESOLVER).unwrap()
    }

    fn provider() -> Arc<CCIPReadMiddleware<Arc<Provider<Http>>>> {
        let rpc_url = std::env::var("RPC_URL")
            .unwrap_or_else(|_| "https://ethereum.publicnode.com".to_string());
        let provider = Provider::<Http>::try_from(rpc_url).unwrap();
        Arc::new(CCIPReadMiddleware::new(Arc::new(provider)))
    }

    fn raw_provider() -> Arc<Provider<Http>> {
        let rpc_url = std::env::var("RPC_URL")
            .unwrap_or_else(|_| "https://ethereum.publicnode.com".to_string());
        Arc::new(Provider::<Http>::try_from(rpc_url).unwrap())
    }

    fn test_service() -> ENSService {
        ENSService {
            cache: Box::new(PassthroughCacheLayer {}),
            discovery: None,
            rpc: Box::new(SimpleFactory::from(raw_provider())),
            opensea_api_key: std::env::var("OPENSEA_API_KEY").unwrap_or_default(),
            ipfs_gateway: "https://ipfs.io/ipfs/".to_string(),
            arweave_gateway: "https://arweave.net/".to_string(),
            profile_records: Arc::from(Records::default().records),
            profile_chains: Arc::from(Coins::default().coins),
            universal_resolver: universal_resolver_address(),
            max_bulk_size: 10,
            cache_ttl: None,
        }
    }

    async fn resolve_record(name: &str, lookup: ENSLookup) -> String {
        let rpc = provider();
        let res = universal::resolve_universal(
            name,
            &[lookup.clone()],
            &rpc,
            &universal_resolver_address(),
        )
        .await
        .unwrap();

        assert!(res.0[0].success, "{name} {} lookup failed", lookup.name());

        lookup
            .decode(
                &res.0[0].data,
                &LookupState {
                    rpc,
                    opensea_api_key: String::new(),
                    ipfs_gateway: "https://ipfs.io/ipfs/".to_string(),
                    arweave_gateway: "https://arweave.net/".to_string(),
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_resolve_universal() {
        let calldata: Vec<ENSLookup> = vec![
            ENSLookup::Addr,
            ENSLookup::StaticText("com.discord"),
            ENSLookup::StaticText("com.github"),
            ENSLookup::StaticText("com.twitter"),
            ENSLookup::StaticText("org.telegram"),
            ENSLookup::StaticText("location"),
        ];

        let res = universal::resolve_universal(
            "antony.sh",
            &calldata,
            &provider(),
            &universal_resolver_address(),
        )
        .await
        .unwrap();

        let address = ethers_core::abi::decode(&[ParamType::Address], &res.0[0].data)
            .unwrap()
            .first()
            .unwrap()
            .clone()
            .into_address()
            .unwrap();

        let text_response: Vec<String> = res.0[1..]
            .iter()
            .map(|t| {
                ethers_core::abi::decode(&[ParamType::String], &t.data)
                    .unwrap()
                    .first()
                    .unwrap()
                    .clone()
                    .into_string()
                    .unwrap()
            })
            .collect();

        // yes, I did make this test completely dependent on me 😈
        // TODO: make less dependent on a single person

        assert_eq!(
            address,
            Address::from_str("0x2B5c7025998f88550Ef2fEce8bf87935f542C190").unwrap()
        );
        assert_eq!(
            text_response,
            vec![
                "antony.sh",
                "Antony1060",
                "AntonyThe1060",
                "Antony1060",
                "Croatia",
            ]
        );
    }

    #[tokio::test]
    async fn test_resolve_universal_ensv2_integration_case() {
        let res = universal::resolve_universal(
            "ur.integration-tests.eth",
            &[ENSLookup::Addr],
            &provider(),
            &universal_resolver_address(),
        )
        .await
        .unwrap();

        let address = ethers_core::abi::decode(&[ParamType::Address], &res.0[0].data)
            .unwrap()
            .first()
            .unwrap()
            .clone()
            .into_address()
            .unwrap();

        assert_eq!(
            address,
            Address::from_str("0x2222222222222222222222222222222222222222").unwrap()
        );
    }

    #[tokio::test]
    async fn test_ens_resolution_test_cases() {
        let cases = [
            (
                "universal-resolver",
                "ur.integration-tests.eth",
                ENSLookup::Addr,
                "0x2222222222222222222222222222222222222222",
            ),
            (
                "forward-wildcard",
                "moo331.nft-owner.eth",
                ENSLookup::Addr,
                "0x51050ec063d393217B436747617aD1C2285Aeeee",
            ),
            (
                "forward-eth-offchain",
                "test.offchaindemo.eth",
                ENSLookup::Addr,
                "0x779981590E7Ccc0CFAe8040Ce7151324747cDb97",
            ),
            (
                "forward-text-onchain",
                "integration-tests.eth",
                ENSLookup::StaticText("avatar"),
                "https://raw.githubusercontent.com/ensdomains/resolution-tests/refs/heads/main/assets/avatar.svg",
            ),
            (
                "forward-text-offchain",
                "test.offchaindemo.eth",
                ENSLookup::StaticText("description"),
                "asdflkjasdflkjasdf",
            ),
            (
                "forward-contenthash",
                "integration-tests.eth",
                ENSLookup::ContentHash,
                "ipfs://bafybeifx7yeb55armcsxwwitkymga5xf53dxiarykms3ygqic223w5sk3m",
            ),
            (
                "forward-dns-offchain",
                "pokersback.com",
                ENSLookup::Addr,
                "0x534631Bcf33BDb069fB20A93d2fdb9e4D4dD42CF",
            ),
        ];

        for (id, name, lookup, expected) in cases {
            let actual = resolve_record(name, lookup).await;
            if expected.starts_with("0x") && expected.len() == 42 {
                assert_eq!(
                    Address::from_str(&actual).unwrap(),
                    Address::from_str(expected).unwrap(),
                    "{id}"
                );
            } else {
                assert_eq!(actual, expected, "{id}");
            }
        }

        let base_address = resolve_record(
            "coins.integration-tests.eth",
            ENSLookup::Multicoin(CoinType::from(BASE_COIN_TYPE)),
        )
        .await;
        assert_eq!(
            Address::from_str(&base_address).unwrap(),
            Address::from_str("0xa66E90D515F576f49Af2dF40952476D56F72A420").unwrap(),
            "forward-base-onchain"
        );

        let eth_reverse = resolve_reverse(
            &provider(),
            &Address::from_str("0xeE9eeaAB0Bb7D9B969D701f6f8212609EDeA252E").unwrap(),
            &universal_resolver_address(),
        )
        .await
        .unwrap();
        assert_eq!(eth_reverse, "devrel.enslabs.eth", "reverse-eth");
    }

    #[tokio::test]
    #[ignore = "L2 primary name reverse resolution is not currently supported by this ethers-based resolver path"]
    async fn test_ens_resolution_reverse_l2_case() {
        let base_reverse = resolve_reverse_coin_type(
            &provider(),
            &Address::from_str("0xa66E90D515F576f49Af2dF40952476D56F72A420").unwrap(),
            BASE_COIN_TYPE,
            &universal_resolver_address(),
        )
        .await
        .unwrap();
        assert_eq!(base_reverse, "coins.integration-tests.eth", "reverse-l2");
    }

    #[tokio::test]
    async fn test_print_ur_integration_tests_profile() {
        let name = "ur.integration-tests.eth";
        let profile = test_service()
            .resolve_profile(LookupInfo::Name(name.to_string()), true)
            .await
            .unwrap();

        let payload = json!({
            "response_length": 1,
            "response": [
                {
                    "type": "success",
                    "name": profile.name,
                    "address": profile.address,
                    "avatar": profile.avatar,
                    "header": profile.header,
                    "contenthash": profile.contenthash,
                    "display": profile.display,
                    "records": profile.records,
                    "chains": profile.chains,
                    "fresh": profile.fresh,
                    "resolver": profile.resolver,
                    "ccip_urls": profile.ccip_urls,
                    "errors": profile.errors,
                }
            ],
        });

        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    }

    #[tokio::test]
    async fn test_print_ur_integration_tests_profile_includes_expected_evm_chains() {
        let profile = test_service()
            .resolve_profile(
                LookupInfo::Name("ur.integration-tests.eth".to_string()),
                true,
            )
            .await
            .unwrap();

        let expected_address = "0x2222222222222222222222222222222222222222".to_string();

        for chain in ["eth", "op", "arb1", "base", "matic", "linea", "scr", "celo"] {
            assert_eq!(
                profile.chains.get(chain),
                Some(&expected_address),
                "{chain}"
            );
        }
    }
}
