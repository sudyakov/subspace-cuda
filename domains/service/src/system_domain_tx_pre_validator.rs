use domain_client_executor::state_root_extractor::StateRootExtractor;
use domain_client_executor::xdm_verifier::verify_xdm_with_primary_chain_client;
use futures::channel::oneshot;
use futures::future::FutureExt;
use sc_transaction_pool::error::Result as TxPoolResult;
use sc_transaction_pool_api::error::Error as TxPoolError;
use sc_transaction_pool_api::TransactionSource;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::traits::SpawnNamed;
use sp_domains::transaction::{
    InvalidTransactionCode, PreValidationObject, PreValidationObjectApi,
};
use sp_domains::ExecutorApi;
use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_runtime::transaction_validity::UnknownTransaction;
use std::marker::PhantomData;
use std::sync::Arc;
use subspace_fraud_proof::VerifyFraudProof;
use subspace_transaction_pool::PreValidateTransaction;

pub struct SystemDomainTxPreValidator<Block, PBlock, Client, Verifier, PClient, SRE> {
    client: Arc<Client>,
    spawner: Box<dyn SpawnNamed>,
    fraud_proof_verifier: Verifier,
    primary_chain_client: Arc<PClient>,
    state_root_extractor: SRE,
    _phantom_data: PhantomData<(Block, PBlock)>,
}

impl<Block, PBlock, Client, Verifier, PClient, SRE> Clone
    for SystemDomainTxPreValidator<Block, PBlock, Client, Verifier, PClient, SRE>
where
    Verifier: Clone,
    SRE: Clone,
{
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            spawner: self.spawner.clone(),
            fraud_proof_verifier: self.fraud_proof_verifier.clone(),
            primary_chain_client: self.primary_chain_client.clone(),
            state_root_extractor: self.state_root_extractor.clone(),
            _phantom_data: self._phantom_data,
        }
    }
}

impl<Block, PBlock, Client, Verifier, PClient, SRE>
    SystemDomainTxPreValidator<Block, PBlock, Client, Verifier, PClient, SRE>
{
    pub fn new(
        client: Arc<Client>,
        spawner: Box<dyn SpawnNamed>,
        fraud_proof_verifier: Verifier,
        primary_chain_client: Arc<PClient>,
        state_root_extractor: SRE,
    ) -> Self {
        Self {
            client,
            spawner,
            fraud_proof_verifier,
            primary_chain_client,
            state_root_extractor,
            _phantom_data: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl<Block, PBlock, Client, Verifier, PClient, SRE> PreValidateTransaction
    for SystemDomainTxPreValidator<Block, PBlock, Client, Verifier, PClient, SRE>
where
    Block: BlockT,
    PBlock: BlockT,
    PBlock::Hash: From<Block::Hash>,
    NumberFor<PBlock>: From<NumberFor<Block>>,
    Client: ProvideRuntimeApi<Block> + Send + Sync,
    Client::Api: PreValidationObjectApi<Block, domain_runtime_primitives::Hash>,
    Verifier: VerifyFraudProof<Block> + Clone + Send + Sync + 'static,
    PClient: HeaderBackend<PBlock> + ProvideRuntimeApi<PBlock> + 'static,
    PClient::Api: ExecutorApi<PBlock, Block::Hash>,
    SRE: StateRootExtractor<Block> + Send + Sync,
{
    type Block = Block;
    async fn pre_validate_transaction(
        &self,
        at: Block::Hash,
        _source: TransactionSource,
        uxt: Block::Extrinsic,
    ) -> TxPoolResult<()> {
        if !verify_xdm_with_primary_chain_client::<PClient, PBlock, Block, SRE>(
            &self.primary_chain_client,
            &self.state_root_extractor,
            &uxt,
        )? {
            return Err(TxPoolError::ImmediatelyDropped.into());
        }

        let pre_validation_object = self
            .client
            .runtime_api()
            .extract_pre_validation_object(at, uxt.clone())
            .map_err(|err| sc_transaction_pool::error::Error::Blockchain(err.into()))?;

        match pre_validation_object {
            PreValidationObject::Null | PreValidationObject::Bundle(_) => {
                // No pre-validation is required.
            }
            PreValidationObject::FraudProof(fraud_proof) => {
                let spawner = self.spawner.clone();
                let fraud_proof_verifier = self.fraud_proof_verifier.clone();

                let (verified_result_sender, verified_result_receiver) = oneshot::channel();

                // Verify the fraud proof in another blocking task as it might be pretty heavy.
                spawner.spawn_blocking(
                    "txpool-fraud-proof-verification",
                    None,
                    async move {
                        let verified_result = fraud_proof_verifier.verify_fraud_proof(&fraud_proof);
                        verified_result_sender
                            .send(verified_result)
                            .expect("Failed to send the verified fraud proof result");
                    }
                    .boxed(),
                );

                match verified_result_receiver.await {
                    Ok(verified_result) => {
                        match verified_result {
                            Ok(_) => {
                                // Continue the regular `validate_transaction`
                            }
                            Err(err) => {
                                tracing::debug!(target: "txpool", error = ?err, "Invalid fraud proof");
                                return Err(TxPoolError::InvalidTransaction(
                                    InvalidTransactionCode::FraudProof.into(),
                                )
                                .into());
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(target: "txpool", error = ?err, "Failed to receive the fraud proof verified result");
                        return Err(TxPoolError::UnknownTransaction(
                            UnknownTransaction::CannotLookup,
                        )
                        .into());
                    }
                }
            }
        }

        Ok(())
    }
}
