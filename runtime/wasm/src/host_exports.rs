use crate::gas::{self, complexity, Gas, GasCounter};
use crate::{error::DeterminismLevel, module::IntoTrap, UnresolvedContractCall};
use ethabi::param_type::Reader;
use ethabi::{decode, encode, Address, Token};
use graph::components::store::EntityKey;
use graph::components::subgraph::{ProofOfIndexingEvent, SharedProofOfIndexing};
use graph::components::three_box::ThreeBoxAdapter;
use graph::components::{arweave::ArweaveAdapter, store::EntityType};
use graph::data::store;
use graph::prelude::serde_json;
use graph::prelude::{slog::b, slog::record_static, *};
use graph::runtime::DeterministicHostError;
use graph::{blockchain::DataSource, bytes::Bytes};
use graph_chain_ethereum::{EthereumAdapterTrait, EthereumContractCall, EthereumContractCallError};
use never::Never;
use semver::Version;
use std::collections::HashMap;
use std::ops::Deref;
use std::str::FromStr;
use std::time::{Duration, Instant};
use web3::types::H160;

use graph::ensure;
use graph_graphql::prelude::validate_entity;
use wasmtime::Trap;

use crate::module::{WasmInstance, WasmInstanceContext};

pub(crate) enum EthereumCallError {
    /// We might have detected a reorg.
    PossibleReorg(anyhow::Error),
    Unknown(anyhow::Error),
    DeterministicHostError(DeterministicHostError),
}

impl From<anyhow::Error> for EthereumCallError {
    fn from(e: anyhow::Error) -> Self {
        EthereumCallError::Unknown(e)
    }
}

impl From<DeterministicHostError> for EthereumCallError {
    fn from(e: DeterministicHostError) -> Self {
        Self::DeterministicHostError(e)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum HostExportError {
    #[error("{0:#}")]
    Unknown(anyhow::Error),

    #[error("{0:#}")]
    Deterministic(anyhow::Error),
}

impl From<anyhow::Error> for HostExportError {
    fn from(e: anyhow::Error) -> Self {
        HostExportError::Unknown(e)
    }
}

impl IntoTrap for HostExportError {
    fn determinism_level(&self) -> DeterminismLevel {
        match self {
            HostExportError::Deterministic(_) => DeterminismLevel::Deterministic,
            HostExportError::Unknown(_) => DeterminismLevel::Unimplemented,
        }
    }
    fn into_trap(self) -> Trap {
        self.into()
    }
}

impl From<HostExportError> for Trap {
    fn from(e: HostExportError) -> Trap {
        match e {
            HostExportError::Unknown(e) | HostExportError::Deterministic(e) => Trap::from(e),
        }
    }
}

pub(crate) struct HostExports {
    pub(crate) subgraph_id: DeploymentHash,
    pub(crate) api_version: Version,
    data_source_name: String,
    data_source_address: Option<Address>,
    data_source_network: String,
    data_source_context: Arc<Option<DataSourceContext>>,
    /// Some data sources have indeterminism or different notions of time. These
    /// need to be each be stored separately to separate causality between them,
    /// and merge the results later. Right now, this is just the ethereum
    /// networks but will be expanded for ipfs and the availability chain.
    causality_region: String,
    templates: Arc<Vec<DataSourceTemplate>>,
    abis: Vec<Arc<MappingABI>>,
    ethereum_adapter: Arc<dyn EthereumAdapterTrait>,
    pub(crate) link_resolver: Arc<dyn LinkResolver>,
    call_cache: Arc<dyn EthereumCallCache>,
    store: Arc<dyn crate::RuntimeStore>,
    arweave_adapter: Arc<dyn ArweaveAdapter>,
    three_box_adapter: Arc<dyn ThreeBoxAdapter>,
}

// Not meant to be useful, only to allow deriving.
impl std::fmt::Debug for HostExports {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "HostExports",)
    }
}

impl HostExports {
    pub(crate) fn new(
        subgraph_id: DeploymentHash,
        data_source: &impl DataSource,
        data_source_network: String,
        templates: Arc<Vec<DataSourceTemplate>>,
        ethereum_adapter: Arc<dyn EthereumAdapterTrait>,
        link_resolver: Arc<dyn LinkResolver>,
        store: Arc<dyn crate::RuntimeStore>,
        call_cache: Arc<dyn EthereumCallCache>,
        arweave_adapter: Arc<dyn ArweaveAdapter>,
        three_box_adapter: Arc<dyn ThreeBoxAdapter>,
    ) -> Self {
        let causality_region = format!("ethereum/{}", data_source_network);

        Self {
            subgraph_id,
            api_version: data_source.mapping().api_version.clone(),
            data_source_name: data_source.name().to_owned(),
            data_source_address: data_source.source().address.clone(),
            data_source_network,
            data_source_context: data_source.context().cheap_clone(),
            causality_region,
            templates,
            abis: data_source.mapping().abis.clone(),
            ethereum_adapter,
            link_resolver,
            call_cache,
            store,
            arweave_adapter,
            three_box_adapter,
        }
    }

    pub(crate) fn abort(
        &self,
        message: Option<String>,
        file_name: Option<String>,
        line_number: Option<u32>,
        column_number: Option<u32>,
        gas: &GasCounter,
    ) -> Result<Never, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_BASE_COST.into())?;

        let message = message
            .map(|message| format!("message: {}", message))
            .unwrap_or_else(|| "no message".into());
        let location = match (file_name, line_number, column_number) {
            (None, None, None) => "an unknown location".into(),
            (Some(file_name), None, None) => file_name,
            (Some(file_name), Some(line_number), None) => {
                format!("{}, line {}", file_name, line_number)
            }
            (Some(file_name), Some(line_number), Some(column_number)) => format!(
                "{}, line {}, column {}",
                file_name, line_number, column_number
            ),
            _ => unreachable!(),
        };
        Err(DeterministicHostError(anyhow::anyhow!(
            "Mapping aborted at {}, with {}",
            location,
            message
        )))
    }

    pub(crate) fn store_set(
        &self,
        logger: &Logger,
        state: &mut BlockState,
        proof_of_indexing: &SharedProofOfIndexing,
        entity_type: String,
        entity_id: String,
        mut data: HashMap<String, Value>,
        stopwatch: &StopwatchMetrics,
        gas: &GasCounter,
    ) -> Result<(), anyhow::Error> {
        let poi_section = stopwatch.start_section("host_export_store_set__proof_of_indexing");
        if let Some(proof_of_indexing) = proof_of_indexing {
            let mut proof_of_indexing = proof_of_indexing.deref().borrow_mut();
            proof_of_indexing.write(
                logger,
                &self.causality_region,
                &ProofOfIndexingEvent::SetEntity {
                    entity_type: &entity_type,
                    id: &entity_id,
                    data: &data,
                },
            );
        }
        poi_section.end();

        let id_insert_section = stopwatch.start_section("host_export_store_set__insert_id");
        // Automatically add an "id" value
        match data.insert("id".to_string(), Value::String(entity_id.clone())) {
            Some(ref v) if v != &Value::String(entity_id.clone()) => {
                return Err(anyhow!(
                    "Value of {} attribute 'id' conflicts with ID passed to `store.set()`: \
                     {} != {}",
                    entity_type,
                    v,
                    entity_id,
                ));
            }
            _ => (),
        }

        id_insert_section.end();
        let validation_section = stopwatch.start_section("host_export_store_set__validation");
        let key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type),
            entity_id,
        };

        gas.consume_host_fn(gas::STORE_SET.with_args(complexity::Linear, (&key, &data)))?;

        let entity = Entity::from(data);
        let schema = self.store.input_schema(&self.subgraph_id)?;
        let is_valid = validate_entity(&schema.document, &key, &entity).is_ok();
        state.entity_cache.set(key.clone(), entity);

        validation_section.end();
        // Validate the changes against the subgraph schema.
        // If the set of fields we have is already valid, avoid hitting the DB.
        if !is_valid {
            stopwatch.start_section("host_export_store_set__post_validation");
            let entity = state
                .entity_cache
                .get(&key)
                .map_err(|e| HostExportError::Unknown(e.into()))?
                .expect("we just stored this entity");
            validate_entity(&schema.document, &key, &entity)?;
        }
        Ok(())
    }

    pub(crate) fn store_remove(
        &self,
        logger: &Logger,
        state: &mut BlockState,
        proof_of_indexing: &SharedProofOfIndexing,
        entity_type: String,
        entity_id: String,
        gas: &GasCounter,
    ) -> Result<(), HostExportError> {
        if let Some(proof_of_indexing) = proof_of_indexing {
            let mut proof_of_indexing = proof_of_indexing.deref().borrow_mut();
            proof_of_indexing.write(
                logger,
                &self.causality_region,
                &ProofOfIndexingEvent::RemoveEntity {
                    entity_type: &entity_type,
                    id: &entity_id,
                },
            );
        }
        let key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type),
            entity_id,
        };

        gas.consume_host_fn(gas::STORE_REMOVE.with_args(complexity::Size, &key))?;

        state.entity_cache.remove(key);

        Ok(())
    }

    pub(crate) fn store_get(
        &self,
        state: &mut BlockState,
        entity_type: String,
        entity_id: String,
        gas: &GasCounter,
    ) -> Result<Option<Entity>, anyhow::Error> {
        let store_key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type.clone()),
            entity_id: entity_id.clone(),
        };

        let result = state.entity_cache.get(&store_key)?;
        gas.consume_host_fn(gas::STORE_GET.with_args(complexity::Linear, (&store_key, &result)))?;

        Ok(state.entity_cache.get(&store_key)?)
    }

    /// Returns `Ok(None)` if the call was reverted.
    pub(crate) fn ethereum_call(
        &self,
        logger: &Logger,
        block_ptr: &BlockPtr,
        unresolved_call: UnresolvedContractCall,
        gas: &GasCounter,
    ) -> Result<Option<Vec<Token>>, EthereumCallError> {
        gas.consume_host_fn(gas::ETHEREUM_CALL)?;

        let start_time = Instant::now();

        // Obtain the path to the contract ABI
        let contract = self
            .abis
            .iter()
            .find(|abi| abi.name == unresolved_call.contract_name)
            .with_context(|| {
                format!(
                    "Could not find ABI for contract \"{}\", try adding it to the 'abis' section \
                     of the subgraph manifest",
                    unresolved_call.contract_name
                )
            })?
            .contract
            .clone();

        let function = match unresolved_call.function_signature {
            // Behavior for apiVersion < 0.0.4: look up function by name; for overloaded
            // functions this always picks the same overloaded variant, which is incorrect
            // and may lead to encoding/decoding errors
            None => contract
                .function(unresolved_call.function_name.as_str())
                .with_context(|| {
                    format!(
                        "Unknown function \"{}::{}\" called from WASM runtime",
                        unresolved_call.contract_name, unresolved_call.function_name
                    )
                })?,

            // Behavior for apiVersion >= 0.0.04: look up function by signature of
            // the form `functionName(uint256,string) returns (bytes32,string)`; this
            // correctly picks the correct variant of an overloaded function
            Some(ref function_signature) => contract
                .functions_by_name(unresolved_call.function_name.as_str())
                .with_context(|| {
                    format!(
                        "Unknown function \"{}::{}\" called from WASM runtime",
                        unresolved_call.contract_name, unresolved_call.function_name
                    )
                })?
                .iter()
                .find(|f| function_signature == &f.signature())
                .with_context(|| {
                    format!(
                        "Unknown function \"{}::{}\" with signature `{}` \
                         called from WASM runtime",
                        unresolved_call.contract_name,
                        unresolved_call.function_name,
                        function_signature,
                    )
                })?,
        };

        let call = EthereumContractCall {
            address: unresolved_call.contract_address.clone(),
            block_ptr: block_ptr.cheap_clone(),
            function: function.clone(),
            args: unresolved_call.function_args.clone(),
        };

        // Run Ethereum call in tokio runtime
        let eth_adapter = self.ethereum_adapter.clone();
        let logger1 = logger.clone();
        let call_cache = self.call_cache.clone();
        let result = match block_on(future::lazy(move || {
            eth_adapter.contract_call(&logger1, call, call_cache)
        })) {
            Ok(tokens) => Ok(Some(tokens)),
            Err(EthereumContractCallError::Revert(reason)) => {
                info!(logger, "Contract call reverted"; "reason" => reason);
                Ok(None)
            }

            // Any error reported by the Ethereum node could be due to the block no longer being on
            // the main chain. This is very unespecific but we don't want to risk failing a
            // subgraph due to a transient error such as a reorg.
            Err(EthereumContractCallError::Web3Error(e)) => Err(EthereumCallError::PossibleReorg(anyhow::anyhow!(
                "Ethereum node returned an error when calling function \"{}\" of contract \"{}\": {}",
                unresolved_call.function_name,
                unresolved_call.contract_name,
                e
            ))),

            // Also retry on timeouts.
            Err(EthereumContractCallError::Timeout) => Err(EthereumCallError::PossibleReorg(anyhow::anyhow!(
                "Ethereum node did not respond when calling function \"{}\" of contract \"{}\"",
                unresolved_call.function_name,
                unresolved_call.contract_name,
            ))),

            Err(e) => Err(EthereumCallError::Unknown(anyhow::anyhow!(
                "Failed to call function \"{}\" of contract \"{}\": {}",
                unresolved_call.function_name,
                unresolved_call.contract_name,
                e
            ))),
        };

        trace!(logger, "Contract call finished";
              "address" => &unresolved_call.contract_address.to_string(),
              "contract" => &unresolved_call.contract_name,
              "function" => &unresolved_call.function_name,
              "function_signature" => &unresolved_call.function_signature,
              "time" => format!("{}ms", start_time.elapsed().as_millis()));

        result.map_err(Into::into)
    }

    /// Prints the module of `n` in hex.
    /// Integers are encoded using the least amount of digits (no leading zero digits).
    /// Their encoding may be of uneven length. The number zero encodes as "0x0".
    ///
    /// https://godoc.org/github.com/ethereum/go-ethereum/common/hexutil#hdr-Encoding_Rules
    pub(crate) fn big_int_to_hex(
        &self,
        n: BigInt,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &n))?;

        if n == 0.into() {
            return Ok("0x0".to_string());
        }

        let bytes = n.to_bytes_be().1;
        Ok(format!(
            "0x{}",
            ::hex::encode(bytes).trim_start_matches('0')
        ))
    }

    pub(crate) fn ipfs_cat(&self, logger: &Logger, link: String) -> Result<Vec<u8>, anyhow::Error> {
        // Does not consume gas because this is not a part of the deterministic feature set.
        // Ideally this would first consume gas for fetching the file stats, and then again
        // for the bytes of the file.
        block_on03(self.link_resolver.cat(logger, &Link { link }))
    }

    // Read the IPFS file `link`, split it into JSON objects, and invoke the
    // exported function `callback` on each JSON object. The successful return
    // value contains the block state produced by each callback invocation. Each
    // invocation of `callback` happens in its own instance of a WASM module,
    // which is identical to `module` when it was first started. The signature
    // of the callback must be `callback(JSONValue, Value)`, and the `userData`
    // parameter is passed to the callback without any changes
    pub(crate) fn ipfs_map(
        link_resolver: &Arc<dyn LinkResolver>,
        module: &mut WasmInstanceContext,
        link: String,
        callback: &str,
        user_data: store::Value,
        flags: Vec<String>,
    ) -> Result<Vec<BlockState>, anyhow::Error> {
        const JSON_FLAG: &str = "json";
        ensure!(
            flags.contains(&JSON_FLAG.to_string()),
            "Flags must contain 'json'"
        );

        let host_metrics = module.host_metrics.clone();
        let valid_module = module.valid_module.clone();
        let ctx = module.ctx.derive_with_empty_block_state();
        let callback = callback.to_owned();
        // Create a base error message to avoid borrowing headaches
        let errmsg = format!(
            "ipfs_map: callback '{}' failed when processing file '{}'",
            &*callback, &link
        );

        let start = Instant::now();
        let mut last_log = start;
        let logger = ctx.logger.new(o!("ipfs_map" => link.clone()));

        let result = {
            let mut stream: JsonValueStream =
                block_on03(link_resolver.json_stream(&logger, &Link { link }))?;
            let mut v = Vec::new();
            while let Some(sv) = block_on03(stream.next()) {
                let sv = sv?;
                let module = WasmInstance::from_valid_module_with_ctx(
                    valid_module.clone(),
                    ctx.derive_with_empty_block_state(),
                    host_metrics.clone(),
                    module.timeout,
                    module.experimental_features.clone(),
                )?;
                let result = module.handle_json_callback(&callback, &sv.value, &user_data)?;
                // Log progress every 15s
                if last_log.elapsed() > Duration::from_secs(15) {
                    debug!(
                        logger,
                        "Processed {} lines in {}s so far",
                        sv.line,
                        start.elapsed().as_secs()
                    );
                    last_log = Instant::now();
                }
                v.push(result)
            }
            Ok(v)
        };
        result.map_err(move |e: Error| anyhow::anyhow!("{}: {}", errmsg, e.to_string()))
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_i64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<i64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;
        i64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as i64", json))
            .map_err(DeterministicHostError)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_u64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<u64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        u64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as u64", json))
            .map_err(DeterministicHostError)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_f64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<f64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        f64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as f64", json))
            .map_err(DeterministicHostError)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_big_int(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<Vec<u8>, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        let big_int = BigInt::from_str(&json)
            .with_context(|| format!("JSON `{}` is not a decimal string", json))
            .map_err(DeterministicHostError)?;
        Ok(big_int.to_signed_bytes_le())
    }

    pub(crate) fn crypto_keccak_256(
        &self,
        input: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<[u8; 32], DeterministicHostError> {
        let data = &input[..];
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, data))?;
        Ok(tiny_keccak::keccak256(data))
    }

    pub(crate) fn big_int_plus(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x + y)
    }

    pub(crate) fn big_int_minus(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x - y)
    }

    pub(crate) fn big_int_times(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        Ok(x * y)
    }

    pub(crate) fn big_int_divided_by(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError(anyhow!(
                "attempted to divide BigInt `{}` by zero",
                x
            )));
        }
        Ok(x / y)
    }

    pub(crate) fn big_int_mod(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError(anyhow!(
                "attempted to calculate the remainder of `{}` with a divisor of zero",
                x
            )));
        }
        Ok(x % y)
    }

    /// Limited to a small exponent to avoid creating huge BigInts.
    pub(crate) fn big_int_pow(
        &self,
        x: BigInt,
        exp: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Exponential, (&x, exp)))?;
        Ok(x.pow(exp))
    }

    pub(crate) fn big_int_from_string(
        &self,
        s: String,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &s))?;
        BigInt::from_str(&s)
            .with_context(|| format!("string is not a BigInt: `{}`", s))
            .map_err(DeterministicHostError)
    }

    pub(crate) fn big_int_bit_or(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x | y)
    }

    pub(crate) fn big_int_bit_and(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Min, (&x, &y)))?;
        Ok(x & y)
    }

    pub(crate) fn big_int_left_shift(
        &self,
        x: BigInt,
        bits: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Linear, (&x, &bits)))?;
        Ok(x << bits)
    }

    pub(crate) fn big_int_right_shift(
        &self,
        x: BigInt,
        bits: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Linear, (&x, &bits)))?;
        Ok(x >> bits)
    }

    /// Useful for IPFS hashes stored as bytes
    pub(crate) fn bytes_to_base58(
        &self,
        bytes: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &bytes))?;
        Ok(::bs58::encode(&bytes).into_string())
    }

    pub(crate) fn big_decimal_plus(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Linear, (&x, &y)))?;
        Ok(x + y)
    }

    pub(crate) fn big_decimal_minus(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Linear, (&x, &y)))?;
        Ok(x - y)
    }

    pub(crate) fn big_decimal_times(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        Ok(x * y)
    }

    /// Maximum precision of 100 decimal digits.
    pub(crate) fn big_decimal_divided_by(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError(anyhow!(
                "attempted to divide BigDecimal `{}` by zero",
                x
            )));
        }
        Ok(x / y)
    }

    pub(crate) fn big_decimal_equals(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<bool, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Min, (&x, &y)))?;
        Ok(x == y)
    }

    pub(crate) fn big_decimal_to_string(
        &self,
        x: BigDecimal,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &x))?;
        Ok(x.to_string())
    }

    pub(crate) fn big_decimal_from_string(
        &self,
        s: String,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &s))?;
        BigDecimal::from_str(&s)
            .with_context(|| format!("string  is not a BigDecimal: '{}'", s))
            .map_err(DeterministicHostError)
    }

    pub(crate) fn data_source_create(
        &self,
        logger: &Logger,
        state: &mut BlockState,
        name: String,
        params: Vec<String>,
        context: Option<DataSourceContext>,
        creation_block: BlockNumber,
        gas: &GasCounter,
    ) -> Result<(), HostExportError> {
        gas.consume_host_fn(gas::CREATE_DATA_SOURCE)?;
        info!(
            logger,
            "Create data source";
            "name" => &name,
            "params" => format!("{}", params.join(","))
        );

        // Resolve the name into the right template
        let template = self
            .templates
            .iter()
            .find(|template| template.name == name)
            .with_context(|| {
                format!(
                    "Failed to create data source from name `{}`: \
                     No template with this name in parent data source `{}`. \
                     Available names: {}.",
                    name,
                    self.data_source_name,
                    self.templates
                        .iter()
                        .map(|template| template.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .map_err(DeterministicHostError)?
            .clone();

        // Remember that we need to create this data source
        state.push_created_data_source(DataSourceTemplateInfo {
            template,
            params,
            context,
            creation_block,
        });

        Ok(())
    }

    pub(crate) fn ens_name_by_hash(&self, hash: &str) -> Result<Option<String>, anyhow::Error> {
        Ok(self.store.find_ens_name(hash)?)
    }

    pub(crate) fn log_log(
        &self,
        logger: &Logger,
        level: slog::Level,
        msg: String,
        gas: &GasCounter,
    ) -> Result<(), DeterministicHostError> {
        gas.consume_host_fn(gas::LOG)?;

        let rs = record_static!(level, self.data_source_name.as_str());

        logger.log(&slog::Record::new(
            &rs,
            &format_args!("{}", msg),
            b!("data_source" => &self.data_source_name),
        ));

        if level == slog::Level::Critical {
            return Err(DeterministicHostError(anyhow!(
                "Critical error logged in mapping"
            )));
        }
        Ok(())
    }

    pub(crate) fn data_source_address(
        &self,
        gas: &GasCounter,
    ) -> Result<H160, DeterministicHostError> {
        gas.consume_host_fn(Gas::from(gas::DEFAULT_BASE_COST))?;
        Ok(self.data_source_address.clone().unwrap_or_default())
    }

    pub(crate) fn data_source_network(
        &self,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(Gas::from(gas::DEFAULT_BASE_COST))?;
        Ok(self.data_source_network.clone())
    }

    pub(crate) fn data_source_context(
        &self,
        gas: &GasCounter,
    ) -> Result<Entity, DeterministicHostError> {
        gas.consume_host_fn(Gas::from(gas::DEFAULT_BASE_COST))?;
        Ok(self
            .data_source_context
            .as_ref()
            .clone()
            .unwrap_or_default())
    }

    pub(crate) fn arweave_transaction_data(&self, tx_id: &str) -> Option<Bytes> {
        block_on03(self.arweave_adapter.tx_data(tx_id)).ok()
    }

    pub(crate) fn box_profile(
        &self,
        address: &str,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        block_on03(self.three_box_adapter.profile(address)).ok()
    }

    pub(crate) fn json_from_bytes(
        &self,
        bytes: &Vec<u8>,
    ) -> Result<serde_json::Value, DeterministicHostError> {
        serde_json::from_reader(bytes.as_slice()).map_err(|e| DeterministicHostError(e.into()))
    }

    pub(crate) fn string_to_h160(
        &self,
        string: &str,
        gas: &GasCounter,
    ) -> Result<H160, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &string))?;
        string_to_h160(string)
    }

    pub(crate) fn bytes_to_string(
        &self,
        logger: &Logger,
        bytes: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &bytes))?;

        Ok(bytes_to_string(logger, bytes))
    }

    pub(crate) fn ethereum_encode(
        &self,
        token: Token,
        gas: &GasCounter,
    ) -> Result<Vec<u8>, DeterministicHostError> {
        let encoded = encode(&[token]);

        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &encoded))?;

        Ok(encoded)
    }

    pub(crate) fn ethereum_decode(
        &self,
        types: String,
        data: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<Token, anyhow::Error> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &data))?;

        let param_types = Reader::read(&types)
            .or_else(|e| Err(anyhow::anyhow!("Failed to read types: {}", e)))?;

        decode(&[param_types], &data)
            // The `.pop().unwrap()` here is ok because we're always only passing one
            // `param_types` to `decode`, so the returned `Vec` has always size of one.
            // We can't do `tokens[0]` because the value can't be moved out of the `Vec`.
            .map(|mut tokens| tokens.pop().unwrap())
            .context("Failed to decode")
    }
}

fn block_on<I, ER>(future: impl Future<Item = I, Error = ER> + Send) -> Result<I, ER> {
    block_on03(future.compat())
}

fn block_on03<T>(future: impl futures03::Future<Output = T> + Send) -> T {
    graph::block_on(future)
}

fn string_to_h160(string: &str) -> Result<H160, DeterministicHostError> {
    // `H160::from_str` takes a hex string with no leading `0x`.
    let s = string.trim_start_matches("0x");
    H160::from_str(s)
        .with_context(|| format!("Failed to convert string to Address/H160: '{}'", s))
        .map_err(DeterministicHostError)
}

fn bytes_to_string(logger: &Logger, bytes: Vec<u8>) -> String {
    let s = String::from_utf8_lossy(&bytes);

    // If the string was re-allocated, that means it was not UTF8.
    if matches!(s, std::borrow::Cow::Owned(_)) {
        warn!(
            logger,
            "Bytes contain invalid UTF8. This may be caused by attempting \
            to convert a value such as an address that cannot be parsed to a unicode string. \
            You may want to use 'toHexString()' instead. String (truncated to 1024 chars): '{}'",
            &s[..s.len().min(1024)],
        )
    }

    // The string may have been encoded in a fixed length buffer and padded with null
    // characters, so trim trailing nulls.
    s.trim_end_matches('\u{0000}').to_string()
}

#[test]
fn test_string_to_h160_with_0x() {
    assert_eq!(
        H160::from_str("A16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap(),
        string_to_h160("0xA16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap()
    )
}

#[test]
fn bytes_to_string_is_lossy() {
    assert_eq!(
        "Downcoin WETH-USDT",
        bytes_to_string(
            &graph::log::logger(true),
            vec![68, 111, 119, 110, 99, 111, 105, 110, 32, 87, 69, 84, 72, 45, 85, 83, 68, 84]
        )
    );

    assert_eq!(
        "Downcoin WETH-USDT�",
        bytes_to_string(
            &graph::log::logger(true),
            vec![
                68, 111, 119, 110, 99, 111, 105, 110, 32, 87, 69, 84, 72, 45, 85, 83, 68, 84, 160,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        )
    )
}
