use anchor_lang::prelude::*;

use crate::{
    constants::{CFG_SEED, DISCRIMINATOR_LEN, MTR_SEED},
    internal::check_and_pay_for_gas,
    state::{Cfg, MessageToRelay},
    RelayerError,
};

#[derive(Accounts)]
#[instruction(mtr_salt: [u8; 32])]
pub struct PayForRelay<'info> {
    /// The account that pays for transaction fees and account creation.
    /// Must be mutable to deduct lamports for account rent and gas fees.
    #[account(mut)]
    pub payer: Signer<'info>,

    /// The relayer config state account that tracks fee parameters.
    /// - Uses PDA with CFG_SEED for deterministic address
    /// - Mutable to update EIP1559 fee data
    #[account(mut, seeds = [CFG_SEED], bump)]
    pub cfg: Account<'info, Cfg>,

    /// The account that receives payment for the gas costs of bridging SOL to Base.
    /// CHECK: This account is validated to be the same as cfg.gas_config.gas_fee_receiver
    #[account(mut, address = cfg.gas_config.gas_fee_receiver @ RelayerError::IncorrectGasFeeReceiver)]
    pub gas_fee_receiver: AccountInfo<'info>,

    #[account(init, payer = payer, seeds = [MTR_SEED, mtr_salt.as_ref()], bump, space = DISCRIMINATOR_LEN + MessageToRelay::INIT_SPACE)]
    pub message_to_relay: Account<'info, MessageToRelay>,

    /// System program required for creating new accounts.
    /// Used internally by Anchor for account initialization.
    pub system_program: Program<'info, System>,
}

pub fn pay_for_relay_handler(
    ctx: Context<PayForRelay>,
    _mtr_salt: [u8; 32],
    outgoing_message: Pubkey,
    gas_limit: u64,
) -> Result<()> {
    check_and_pay_for_gas(
        &ctx.accounts.system_program,
        &ctx.accounts.payer,
        &ctx.accounts.gas_fee_receiver,
        &mut ctx.accounts.cfg,
        gas_limit,
    )?;

    *ctx.accounts.message_to_relay = MessageToRelay {
        nonce: ctx.accounts.cfg.nonce,
        outgoing_message,
        gas_limit,
    };
    ctx.accounts.cfg.nonce += 1;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{setup_relayer, SetupRelayerResult, TEST_GAS_FEE_RECEIVER};
    use crate::{accounts, state::MessageToRelay};
    use anchor_lang::{
        solana_program::{instruction::Instruction, system_program},
        InstructionData,
    };
    use solana_message::Message;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    #[test]
    fn pay_for_relay_initializes_message_and_transfers_gas() {
        let SetupRelayerResult {
            mut svm,
            payer,
            guardian: _,
            cfg_pda,
        } = setup_relayer();
        let payer_pk = payer.pubkey();

        // // Ensure gas fee receiver account exists so system transfer succeeds
        svm.airdrop(&TEST_GAS_FEE_RECEIVER, 1).unwrap();
        let initial_receiver_balance = svm.get_account(&TEST_GAS_FEE_RECEIVER).unwrap().lamports;

        let outgoing_message = Pubkey::new_unique();
        let gas_limit: u64 = 123_456;

        // Derive PDA for message_to_relay using salt
        let mtr_salt = Pubkey::new_unique().to_bytes();
        let (message_to_relay, _) = Pubkey::find_program_address(
            &[crate::constants::MTR_SEED, mtr_salt.as_ref()],
            &crate::ID,
        );

        let accounts = accounts::PayForRelay {
            payer: payer_pk,
            cfg: cfg_pda,
            gas_fee_receiver: TEST_GAS_FEE_RECEIVER,
            message_to_relay,
            system_program: system_program::ID,
        }
        .to_account_metas(None);

        let ix = Instruction {
            program_id: crate::ID,
            accounts,
            data: crate::instruction::PayForRelay {
                mtr_salt,
                outgoing_message,
                gas_limit,
            }
            .data(),
        };

        let tx = Transaction::new(
            &[&payer],
            Message::new(&[ix], Some(&payer_pk)),
            svm.latest_blockhash(),
        );

        svm.send_transaction(tx)
            .expect("failed to send transaction");

        // Assert message account was initialized with expected fields
        let msg_account = svm.get_account(&message_to_relay).unwrap();
        let msg = MessageToRelay::try_deserialize(&mut &msg_account.data[..]).unwrap();
        assert_eq!(msg.outgoing_message, outgoing_message);
        assert_eq!(msg.gas_limit, gas_limit);

        // With base_fee = 1 in tests, gas_cost == gas_limit
        let final_receiver_balance = svm.get_account(&TEST_GAS_FEE_RECEIVER).unwrap().lamports;
        assert_eq!(final_receiver_balance - initial_receiver_balance, gas_limit);
    }
}


#[cfg(test)]
mod zero_target_poc {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use crate::internal::{Eip1559, Eip1559Config};

    fn state(target: u64, denominator: u64, window: u64, gas_used: u64) -> Eip1559 {
        Eip1559 {
            config: Eip1559Config { target, denominator, window_duration_seconds: window, minimum_base_fee: 1 },
            current_base_fee: 100,
            current_window_gas_used: gas_used,
            window_start_time: 1_000,
        }
    }

    fn panics<R>(f: impl FnOnce() -> R) -> bool {
        catch_unwind(AssertUnwindSafe(f)).is_err()
    }

    #[test]
    fn zero_target_expired_window_with_gas_panics() {
        let mut s = state(0, 2, 1, 100_000);
        assert!(panics(|| s.refresh_base_fee(1_010)));
    }

    #[test]
    fn zero_window_duration_panics_even_without_expiry() {
        let mut s = state(5_000_000, 2, 0, 0);
        assert!(panics(|| s.refresh_base_fee(1_000)));
    }

    #[test]
    fn zero_denominator_panics_when_gas_differs_from_target() {
        let mut s = state(5_000_000, 0, 1, 100_000);
        assert!(panics(|| s.refresh_base_fee(1_010)));
    }

    #[test]
    fn valid_config_control_does_not_panic() {
        let mut s = state(5_000_000, 2, 1, 100_000);
        assert!(!panics(|| s.refresh_base_fee(1_010)));
    }

    #[test]
    fn zero_target_zero_gas_control_does_not_panic() {
        let mut s = state(0, 2, 1, 0);
        assert!(!panics(|| s.refresh_base_fee(1_010)));
    }
}
