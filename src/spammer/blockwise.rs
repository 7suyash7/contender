use crate::db::DbOps;
use crate::error::ContenderError;
use crate::generator::named_txs::ExecutionRequest;
use crate::generator::seeder::Seeder;
use crate::generator::templater::Templater;
use crate::generator::types::{AnyProvider, EthProvider, PlanType};
use crate::generator::{Generator, NamedTxRequest, PlanConfig};
use crate::spammer::ExecutionPayload;
use crate::test_scenario::TestScenario;
use crate::Result;
use alloy::hex::ToHexExt;
use alloy::network::{AnyNetwork, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy::primitives::{Address, FixedBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ReqwestClient;
use alloy::rpc::types::TransactionRequest;
use futures::StreamExt;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use tokio::task;

use super::tx_actor::TxActorHandle;
use super::OnTxSent;

pub struct BlockwiseSpammer<F, D, S, P>
where
    D: DbOps + Send + Sync + 'static,
    S: Seeder + Send + Sync,
    F: OnTxSent + Send + Sync + 'static,
    P: PlanConfig<String> + Templater<String> + Send + Sync,
{
    scenario: TestScenario<D, S, P>,
    msg_handler: Arc<TxActorHandle>,
    rpc_client: AnyProvider,
    eth_client: Arc<EthProvider>,
    callback_handler: Arc<F>,
    nonces: HashMap<Address, u64>,
    gas_limits: HashMap<FixedBytes<4>, u128>,
}

impl<F, D, S, P> BlockwiseSpammer<F, D, S, P>
where
    F: OnTxSent + Send + Sync + 'static,
    D: DbOps + Send + Sync + 'static,
    S: Seeder + Send + Sync,
    P: PlanConfig<String> + Templater<String> + Send + Sync,
{
    pub async fn new(scenario: TestScenario<D, S, P>, callback_handler: F) -> Self {
        let rpc_client = ProviderBuilder::new()
            .network::<AnyNetwork>()
            .on_http(scenario.rpc_url.to_owned());
        let eth_client = Arc::new(ProviderBuilder::new().on_http(scenario.rpc_url.to_owned()));
        let msg_handler = Arc::new(TxActorHandle::new(
            12,
            scenario.db.clone(),
            Arc::new(rpc_client.to_owned()),
        ));
        let callback_handler = Arc::new(callback_handler);

        // get nonce for each signer and put it into a hashmap
        let mut nonces = HashMap::new();
        for (addr, _) in scenario.wallet_map.iter() {
            let nonce = eth_client
                .get_transaction_count(*addr)
                .await
                .expect("failed to retrieve nonce");
            nonces.insert(*addr, nonce);
        }

        // track gas limits for each function signature
        let gas_limits = HashMap::<FixedBytes<4>, u128>::new();

        Self {
            scenario,
            rpc_client,
            eth_client,
            callback_handler,
            msg_handler,
            nonces,
            gas_limits,
        }
    }

    async fn prepare_tx_req(
        &mut self,
        tx_req: &TransactionRequest,
        gas_price: u128,
        chain_id: u64,
    ) -> Result<(TransactionRequest, EthereumWallet)> {
        let from = tx_req.from.expect("missing from address");
        let nonce = self
            .nonces
            .get(&from)
            .expect("failed to get nonce")
            .to_owned();
        /*
            Increment nonce assuming the tx will succeed.
            Note: if any tx fails, txs with higher nonces will also fail.
            However, we'll get a fresh nonce next block.
        */
        self.nonces.insert(from.to_owned(), nonce + 1);
        let fn_sig = FixedBytes::<4>::from_slice(
            tx_req
                .input
                .input
                .to_owned()
                .map(|b| b.split_at(4).0.to_owned())
                .expect("invalid function call")
                .as_slice(),
        );
        if !self.gas_limits.contains_key(fn_sig.as_slice()) {
            let gas_limit = self
                .eth_client
                .estimate_gas(&tx_req.to_owned())
                .await
                .map_err(|e| ContenderError::with_err(e, "failed to estimate gas"))?;
            self.gas_limits.insert(fn_sig, gas_limit);
        }
        // query hashmaps for gaslimit & signer of this tx
        let gas_limit = self
            .gas_limits
            .get(&fn_sig)
            .expect("failed to get gas limit")
            .to_owned();
        let signer = self
            .scenario
            .wallet_map
            .get(&from)
            .expect("failed to create signer")
            .to_owned();

        let full_tx = tx_req
            .clone()
            .with_nonce(nonce)
            .with_gas_price(gas_price)
            .with_chain_id(chain_id)
            .with_gas_limit(gas_limit);

        Ok((full_tx, signer))
    }

    pub async fn spam_rpc(
        &mut self,
        txs_per_block: usize,
        num_blocks: usize,
        run_id: Option<u64>,
    ) -> Result<()> {
        // generate tx requests
        let tx_requests = self
            .scenario
            .load_txs(PlanType::Spam(txs_per_block * num_blocks, |_| Ok(None)))
            .await?;
        let tx_req_chunks: Vec<_> = tx_requests
            .chunks(txs_per_block)
            .map(|slice| slice.to_vec())
            .collect();
        let mut block_offset = 0;
        let mut last_block_number = 0;

        // get chain id before we start spamming
        let chain_id = self
            .rpc_client
            .get_chain_id()
            .await
            .map_err(|e| ContenderError::with_err(e, "failed to get chain id"))?;

        // init block stream
        let poller = self
            .rpc_client
            .watch_blocks()
            .await
            .map_err(|e| ContenderError::with_err(e, "failed to create block poller"))?;
        let mut stream = poller
            .into_stream()
            .flat_map(futures::stream::iter)
            .take(num_blocks);

        let mut tasks = vec![];
        // let mut gas_limits = HashMap::<FixedBytes<4>, u128>::new();

        // // get nonce for each signer and put it into a hashmap
        // let mut nonces = HashMap::new();
        // for (addr, _) in self.scenario.wallet_map.iter() {
        //     let nonce = self
        //         .rpc_client
        //         .get_transaction_count(*addr)
        //         .await
        //         .map_err(|_| ContenderError::SpamError("failed to get nonce", None))?;
        //     nonces.insert(*addr, nonce);
        // }

        while let Some(block_hash) = stream.next().await {
            let block_txs = tx_req_chunks[block_offset].clone();
            block_offset += 1;

            let block = self
                .rpc_client
                .get_block_by_hash(block_hash, alloy::rpc::types::BlockTransactionsKind::Hashes)
                .await
                .map_err(|e| ContenderError::with_err(e, "failed to get block"))?
                .ok_or(ContenderError::SpamError("no block found", None))?;
            last_block_number = block.header.number;

            // get gas price
            let gas_price = self
                .rpc_client
                .get_gas_price()
                .await
                .map_err(|e| ContenderError::with_err(e, "failed to get gas price"))?;

            for (idx, tx) in block_txs.into_iter().enumerate() {
                let gas_price = gas_price + (idx as u128 * 1e9 as u128);
                // clone muh Arcs
                let eth_client = self.eth_client.clone();
                let callback_handler = self.callback_handler.clone();
                let tx_handler = self.msg_handler.clone();

                // prepare tx/bundle with nonce, gas price, signatures, etc
                let payload = match tx {
                    ExecutionRequest::Bundle(reqs) => {
                        // prepare each tx in the bundle (increment nonce, set gas price, etc)
                        let mut bundle_txs = vec![];
                        for req in reqs.iter() {
                            let tx_req = req.tx;
                            let (tx_req, signer) = self
                                .prepare_tx_req(&tx_req, gas_price, chain_id)
                                .await
                                .map_err(|e| ContenderError::with_err(e, "failed to prepare tx"))?;

                            // sign tx
                            let tx_envelope = tx_req.build(&signer).await.map_err(|e| {
                                ContenderError::with_err(e, "bad request: failed to build tx")
                            })?;
                            bundle_txs.push(tx_envelope);
                        }
                        // TODO: call eth_sendBundle with signed txs
                        ExecutionPayload::SignedTxBundle(bundle_txs)
                    }
                    ExecutionRequest::Tx(req) => {
                        let tx_req = req.tx;
                        println!(
                            "sending tx. from={} to={} input={}",
                            tx_req.from.map(|s| s.encode_hex()).unwrap_or_default(),
                            tx_req
                                .to
                                .map(|s| s.to().map(|s| *s))
                                .flatten()
                                .map(|s| s.encode_hex())
                                .unwrap_or_default(),
                            tx_req
                                .input
                                .input
                                .as_ref()
                                .map(|s| s.encode_hex())
                                .unwrap_or_default(),
                        );

                        let (tx_req, signer) = self
                            .prepare_tx_req(&tx_req, gas_price, chain_id)
                            .await
                            .map_err(|e| ContenderError::with_err(e, "failed to prepare tx"))?;

                        // sign tx
                        let tx_envelope = tx_req.build(&signer).await.map_err(|e| {
                            ContenderError::with_err(e, "bad request: failed to build tx")
                        })?;

                        ExecutionPayload::SignedTx(tx_envelope)
                    }
                };

                // build, sign, and send tx/bundle in a new task (green thread)
                tasks.push(task::spawn(async move {
                    let start_timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("failed to get timestamp")
                        .as_millis() as usize;

                    match payload {
                        ExecutionPayload::SignedTx(tx) => {
                            let provider = ProviderBuilder::new().on_provider(eth_client);
                            provider
                                .send_tx_envelope(tx)
                                .await
                                .expect("RPC error: failed to send tx");
                        }
                        ExecutionPayload::SignedTxBundle(txs) => {
                            let bprovider = ReqwestClient::new_http(self.scenario.rpc_url);
                            bprovider.request("eth_sendBundle", (txs,))
                        }
                    }
                    let res = provider
                        .send_transaction(tx_req)
                        .await
                        .expect("failed to send tx");

                    let mut extra = HashMap::new();
                    extra.insert("start_timestamp".to_owned(), start_timestamp.to_string());
                    if let Some(kind) = req.kind.to_owned() {
                        extra.insert("kind".to_owned(), kind);
                    }

                    let maybe_handle = callback_handler.on_tx_sent(
                        res.into_inner(),
                        req,
                        extra.into(),
                        Some(tx_handler),
                    );
                    if let Some(handle) = maybe_handle {
                        handle.await.expect("callback task failed");
                    }
                    // ignore None values so we don't attempt to await them
                }));
            }

            println!("new block: {block_hash}");
            if let Some(run_id) = run_id {
                // write this block's txs to DB
                let _ = self
                    .msg_handler
                    .flush_cache(run_id, last_block_number)
                    .await
                    .map_err(|e| ContenderError::with_err(e.deref(), "failed to flush cache"))?;
            }
        }

        for task in tasks {
            let _ = task.await;
        }

        // wait until there are no txs left in the cache, or until we time out
        let mut timeout_counter = 0;
        if let Some(run_id) = run_id {
            loop {
                timeout_counter += 1;
                if timeout_counter > 12 {
                    println!("Quitting due to timeout.");
                    break;
                }
                let cache_size = self
                    .msg_handler
                    .flush_cache(run_id, last_block_number)
                    .await
                    .map_err(|e| ContenderError::with_err(e.deref(), "failed to empty cache"))?;
                if cache_size == 0 {
                    break;
                }
                last_block_number += 1;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        db::MockDb,
        generator::util::test::spawn_anvil,
        spammer::util::test::{get_test_signers, MockCallback},
        test_scenario::tests::MockConfig,
    };

    use super::*;

    #[tokio::test]
    async fn watches_blocks_and_spams_them() {
        let anvil = spawn_anvil();
        println!("anvil url: {}", anvil.endpoint_url());
        let seed = crate::generator::RandSeed::from_str("444444444444");
        let scenario = TestScenario::new(
            MockConfig,
            MockDb.into(),
            anvil.endpoint_url(),
            seed,
            &get_test_signers(),
        );
        let callback_handler = MockCallback;
        let spammer = BlockwiseSpammer::new(scenario, callback_handler);

        let result = spammer.spam_rpc(10, 3, None).await;
        println!("{:?}", result);
        assert!(result.is_ok());
    }
}
