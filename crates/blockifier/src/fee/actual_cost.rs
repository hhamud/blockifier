use std::iter;
use std::sync::Arc;

use cairo_vm::vm::runners::cairo_runner::ExecutionResources;
use starknet_api::core::ContractAddress;
use starknet_api::transaction::Fee;

use crate::abi::constants as abi_constants;
use crate::context::TransactionContext;
use crate::execution::call_info::CallInfo;
use crate::execution::contract_class::ClassInfo;
use crate::state::cached_state::{CachedState, StateChanges, StateChangesCount};
use crate::state::state_api::{StateReader, StateResult};
use crate::transaction::objects::{
    GasVector, HasRelatedFeeType, ResourcesMapping, StarknetResources, TransactionExecutionResult,
};
use crate::transaction::transaction_types::TransactionType;
use crate::transaction::transaction_utils::calculate_tx_resources;

#[cfg(test)]
#[path = "actual_cost_test.rs"]
pub mod test;

// TODO(Gilad): Use everywhere instead of passing the `actual_{fee,resources}` tuple, which often
// get passed around together.
#[derive(Default)]
pub struct ActualCost {
    pub actual_fee: Fee,
    pub da_gas: GasVector,
    pub actual_resources: ResourcesMapping,
}

impl ActualCost {
    pub fn builder_for_l1_handler<'a>(
        tx_context: Arc<TransactionContext>,
        l1_handler_payload_size: usize,
    ) -> TransactionExecutionResult<ActualCostBuilder<'a>> {
        let signature_length = 0; // Signature is validated on L1.
        Ok(ActualCostBuilder::new(
            tx_context,
            TransactionType::L1Handler,
            l1_handler_payload_size,
            signature_length,
        )?
        .without_sender_address()
        .with_l1_payload_size(l1_handler_payload_size))
    }
}

#[derive(Debug, Clone)]
// Invariant: private fields initialized after `new` is called via dedicated methods.
pub struct ActualCostBuilder<'a> {
    pub tx_context: Arc<TransactionContext>,
    pub tx_type: TransactionType,
    starknet_resources: StarknetResources,
    validate_call_info: Option<&'a CallInfo>,
    execute_call_info: Option<&'a CallInfo>,
    state_changes: StateChanges,
    sender_address: Option<ContractAddress>,
    n_reverted_steps: usize,
}

impl<'a> ActualCostBuilder<'a> {
    // Recommendation: use constructor from account transaction, or from actual cost, to build this.
    pub fn new(
        tx_context: Arc<TransactionContext>,
        tx_type: TransactionType,
        calldata_length: usize,
        signature_length: usize,
    ) -> TransactionExecutionResult<Self> {
        Ok(Self {
            starknet_resources: StarknetResources::new(
                calldata_length,
                signature_length,
                None,
                StateChangesCount::default(),
                None,
                iter::empty(),
            )?,
            sender_address: Some(tx_context.tx_info.sender_address()),
            tx_context,
            tx_type,
            validate_call_info: None,
            execute_call_info: None,
            state_changes: StateChanges::default(),
            n_reverted_steps: 0,
        })
    }

    pub fn without_sender_address(mut self) -> Self {
        self.sender_address = None;
        self
    }

    /// Calls the `build` method to construct the actual cost object, after feeding the builder
    /// using the setters below.
    /// In addition to actual cost, the method returns the resources the bouncer should take into
    /// account when adding the transaction to the block.
    pub fn build(
        self,
        execution_resources: &ExecutionResources,
    ) -> TransactionExecutionResult<(ActualCost, ResourcesMapping)> {
        self.calculate_actual_fee_and_resources(execution_resources)
    }

    // Setters.

    pub fn with_validate_call_info(mut self, validate_call_info: &'a Option<CallInfo>) -> Self {
        self.validate_call_info = validate_call_info.as_ref();
        self
    }

    pub fn with_execute_call_info(mut self, execute_call_info: &'a Option<CallInfo>) -> Self {
        self.execute_call_info = execute_call_info.as_ref();
        self
    }

    pub fn with_class_info(mut self, class_info: ClassInfo) -> Self {
        self.starknet_resources.set_code_size(Some(&class_info));
        self
    }

    pub fn try_add_state_changes(
        mut self,
        state: &mut CachedState<impl StateReader>,
    ) -> StateResult<Self> {
        let new_state_changes = state.get_actual_state_changes()?;
        self.state_changes = StateChanges::merge(vec![self.state_changes, new_state_changes]);
        Ok(self)
    }

    pub fn with_l1_payload_size(mut self, l1_payload_size: usize) -> Self {
        self.starknet_resources.l1_handler_payload_size = Some(l1_payload_size);
        self
    }

    pub fn with_reverted_steps(mut self, n_reverted_steps: usize) -> Self {
        self.n_reverted_steps = n_reverted_steps;
        self
    }

    // Private methods.

    fn use_kzg_da(&self) -> bool {
        self.tx_context.block_context.block_info.use_kzg_da
    }

    // Construct the actual cost object using all fields that were set in the builder.
    fn calculate_actual_fee_and_resources(
        mut self,
        execution_resources: &ExecutionResources,
    ) -> TransactionExecutionResult<(ActualCost, ResourcesMapping)> {
        let use_kzg_da = self.use_kzg_da();
        self.starknet_resources.state_changes_count = self.state_changes.count_for_fee_charge(
            self.sender_address,
            self.tx_context
                .block_context
                .chain_info
                .fee_token_address(&self.tx_context.tx_info.fee_type()),
        );
        // TODO(Dafna, 1/6/2024): Compute the DA size and pass it instead of state_changes_count.
        let da_gas = self.starknet_resources.get_state_changes_cost(use_kzg_da);
        let non_optional_call_infos =
            self.validate_call_info.into_iter().chain(self.execute_call_info);

        // Set the events and messages resources from the transaction's call infos.
        self.starknet_resources.set_events_and_messages_resources(non_optional_call_infos)?;

        let mut actual_resources = calculate_tx_resources(
            &self.tx_context.block_context.versioned_constants,
            execution_resources,
            self.tx_type,
            &self.starknet_resources,
            use_kzg_da,
        )?;

        // Bouncer resources should not include reverted steps; should include the rest, though.
        let bouncer_resources = actual_resources.clone();

        // Add reverted steps to actual_resources' n_steps for correct fee charge.
        *actual_resources.0.get_mut(&abi_constants::N_STEPS_RESOURCE.to_string()).unwrap() +=
            self.n_reverted_steps;

        let tx_info = &self.tx_context.tx_info;
        let actual_fee = if tx_info.enforce_fee()?
        // L1 handler transactions are not charged an L2 fee but it is compared to the L1 fee.
            || self.tx_type == TransactionType::L1Handler
        {
            tx_info.calculate_tx_fee(&actual_resources, &self.tx_context.block_context)?
        } else {
            Fee(0)
        };

        Ok((ActualCost { actual_fee, da_gas, actual_resources }, bouncer_resources))
    }
}
