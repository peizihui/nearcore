use crate::actions::{apply_rent, check_rent};
use crate::config::{
    safe_gas_to_balance, total_prepaid_gas, tx_cost, RuntimeConfig, TransactionCost,
};
use crate::{ApplyState, VerificationResult};
use near_primitives::account::AccessKeyPermission;
use near_primitives::errors::{
    ActionsValidationError, InvalidAccessKeyError, InvalidTxError, RuntimeError,
};
use near_primitives::transaction::{
    Action, AddKeyAction, DeleteAccountAction, DeployContractAction, FunctionCallAction,
    SignedTransaction,
};
use near_primitives::utils::is_valid_account_id;
use near_store::{get_access_key, get_account, set_access_key, set_account, TrieUpdate};
use near_vm_logic::VMLimitConfig;

/// Verifies the signed transaction on top of given state, charges the rent and transaction fees
/// and balances, and updates the state for the used account and access keys.
pub fn verify_and_charge_transaction(
    config: &RuntimeConfig,
    state_update: &mut TrieUpdate,
    apply_state: &ApplyState,
    signed_transaction: &SignedTransaction,
) -> Result<VerificationResult, RuntimeError> {
    let transaction = &signed_transaction.transaction;
    let signer_id = &transaction.signer_id;
    if !is_valid_account_id(&signer_id) {
        return Err(InvalidTxError::InvalidSigner(signer_id.clone()).into());
    }
    if !is_valid_account_id(&transaction.receiver_id) {
        return Err(InvalidTxError::InvalidReceiver(transaction.receiver_id.clone()).into());
    }

    if !signed_transaction
        .signature
        .verify(signed_transaction.get_hash().as_ref(), &transaction.public_key)
    {
        return Err(InvalidTxError::InvalidSignature.into());
    }

    validate_actions(&config.wasm_config.limit_config, &transaction.actions)
        .map_err(|e| InvalidTxError::ActionsValidation(e))?;

    let mut signer = match get_account(state_update, signer_id)? {
        Some(signer) => signer,
        None => {
            return Err(InvalidTxError::SignerDoesNotExist(signer_id.clone()).into());
        }
    };
    let mut access_key = match get_access_key(state_update, &signer_id, &transaction.public_key)? {
        Some(access_key) => access_key,
        None => {
            return Err(InvalidTxError::InvalidAccessKey(
                InvalidAccessKeyError::AccessKeyNotFound(
                    signer_id.clone(),
                    transaction.public_key.clone(),
                ),
            )
            .into());
        }
    };

    if transaction.nonce <= access_key.nonce {
        return Err(InvalidTxError::InvalidNonce(transaction.nonce, access_key.nonce).into());
    }

    let sender_is_receiver = &transaction.receiver_id == signer_id;

    let rent_paid = apply_rent(&signer_id, &mut signer, apply_state.block_index, &config);
    access_key.nonce = transaction.nonce;

    let TransactionCost { gas_burnt, gas_used, total_cost } =
        tx_cost(&config.transaction_costs, &transaction, apply_state.gas_price, sender_is_receiver)
            .map_err(|_| InvalidTxError::CostOverflow)?;

    signer.amount = signer.amount.checked_sub(total_cost).ok_or_else(|| {
        InvalidTxError::NotEnoughBalance(signer_id.clone(), signer.amount, total_cost)
    })?;

    if let AccessKeyPermission::FunctionCall(ref mut function_call_permission) =
        access_key.permission
    {
        if let Some(ref mut allowance) = function_call_permission.allowance {
            *allowance = allowance.checked_sub(total_cost).ok_or_else(|| {
                InvalidTxError::InvalidAccessKey(InvalidAccessKeyError::NotEnoughAllowance(
                    signer_id.clone(),
                    transaction.public_key.clone(),
                    *allowance,
                    total_cost,
                ))
            })?;
        }
    }

    if let Err(amount) = check_rent(&signer_id, &signer, &config, apply_state.epoch_length) {
        return Err(InvalidTxError::RentUnpaid(signer_id.clone(), amount).into());
    }

    if let AccessKeyPermission::FunctionCall(ref function_call_permission) = access_key.permission {
        if transaction.actions.len() != 1 {
            return Err(InvalidTxError::InvalidAccessKey(InvalidAccessKeyError::ActionError).into());
        }
        if let Some(Action::FunctionCall(ref function_call)) = transaction.actions.get(0) {
            if transaction.receiver_id != function_call_permission.receiver_id {
                return Err(InvalidTxError::InvalidAccessKey(
                    InvalidAccessKeyError::ReceiverMismatch(
                        transaction.receiver_id.clone(),
                        function_call_permission.receiver_id.clone(),
                    ),
                )
                .into());
            }
            if !function_call_permission.method_names.is_empty()
                && function_call_permission
                    .method_names
                    .iter()
                    .all(|method_name| &function_call.method_name != method_name)
            {
                return Err(InvalidTxError::InvalidAccessKey(
                    InvalidAccessKeyError::MethodNameMismatch(function_call.method_name.clone()),
                )
                .into());
            }
        } else {
            return Err(InvalidTxError::InvalidAccessKey(InvalidAccessKeyError::ActionError).into());
        }
    };

    set_access_key(state_update, &signer_id, &transaction.public_key, &access_key);
    set_account(state_update, &signer_id, &signer);

    let validator_reward = safe_gas_to_balance(apply_state.gas_price, gas_burnt)
        .map_err(|_| InvalidTxError::CostOverflow)?;

    Ok(VerificationResult { gas_burnt, gas_used, rent_paid, validator_reward })
}

/// Validates given actions. Checks limits and validates `account_id` if applicable.
pub(crate) fn validate_actions(
    limit_config: &VMLimitConfig,
    actions: &[Action],
) -> Result<(), ActionsValidationError> {
    if actions.len() as u32 > limit_config.max_actions_per_receipt {
        return Err(ActionsValidationError::TotalNumberOfActionsExceeded(
            actions.len() as u32,
            limit_config.max_actions_per_receipt,
        ));
    }

    for action in actions {
        validate_action(limit_config, action)?;
    }

    let total_prepaid_gas =
        total_prepaid_gas(actions).map_err(|_| ActionsValidationError::IntegerOverflow)?;
    if total_prepaid_gas > limit_config.max_total_prepaid_gas {
        return Err(ActionsValidationError::TotalPrepaidGasExceeded(
            total_prepaid_gas,
            limit_config.max_total_prepaid_gas,
        ));
    }

    Ok(())
}

fn validate_action(
    limit_config: &VMLimitConfig,
    action: &Action,
) -> Result<(), ActionsValidationError> {
    match action {
        Action::CreateAccount(_) => Ok(()),
        Action::DeployContract(a) => validate_deploy_contract_action(limit_config, a),
        Action::FunctionCall(a) => validate_function_call_action(limit_config, a),
        Action::Transfer(_) => Ok(()),
        Action::Stake(_) => Ok(()),
        Action::AddKey(a) => validate_add_key_action(limit_config, a),
        Action::DeleteKey(_) => Ok(()),
        Action::DeleteAccount(a) => validate_delete_account_action(a),
    }
}

fn validate_deploy_contract_action(
    limit_config: &VMLimitConfig,
    action: &DeployContractAction,
) -> Result<(), ActionsValidationError> {
    if action.code.len() as u64 > limit_config.max_contract_size {
        return Err(ActionsValidationError::ContractSizeExceeded(
            action.code.len() as u64,
            limit_config.max_contract_size,
        ));
    }

    Ok(())
}

fn validate_function_call_action(
    limit_config: &VMLimitConfig,
    action: &FunctionCallAction,
) -> Result<(), ActionsValidationError> {
    if action.method_name.len() as u64 > limit_config.max_length_method_name {
        return Err(ActionsValidationError::FunctionCallMethodNameLengthExceeded(
            action.method_name.len() as u64,
            limit_config.max_length_method_name,
        ));
    }

    if action.args.len() as u64 > limit_config.max_arguments_length {
        return Err(ActionsValidationError::FunctionCallArgumentsLengthExceeded(
            action.args.len() as u64,
            limit_config.max_arguments_length,
        ));
    }

    Ok(())
}

fn validate_add_key_action(
    limit_config: &VMLimitConfig,
    action: &AddKeyAction,
) -> Result<(), ActionsValidationError> {
    if let AccessKeyPermission::FunctionCall(fc) = &action.access_key.permission {
        if !is_valid_account_id(&fc.receiver_id) {
            return Err(ActionsValidationError::InvalidAccountId(fc.receiver_id.clone()));
        }
        // Checking method name length limits
        let mut num_bytes = 0;
        for method_name in &fc.method_names {
            let len = method_name.len() as u64;
            if len > limit_config.max_length_method_name {
                return Err(ActionsValidationError::AddKeyMethodNameLengthExceeded(
                    len,
                    limit_config.max_length_method_name,
                ));
            }
            // Adding terminating character to total number of bytes
            num_bytes += len + 1;
        }
        if num_bytes > limit_config.max_number_bytes_method_names {
            return Err(ActionsValidationError::AddKeyMethodNamesNumberOfBytesExceeded(
                num_bytes,
                limit_config.max_number_bytes_method_names,
            ));
        }
    }

    Ok(())
}

fn validate_delete_account_action(
    action: &DeleteAccountAction,
) -> Result<(), ActionsValidationError> {
    if !is_valid_account_id(&action.beneficiary_id) {
        return Err(ActionsValidationError::InvalidAccountId(action.beneficiary_id.clone()));
    }

    Ok(())
}
