use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("Spout111111111111111111111111111111111111111");

#[program]
pub mod spout_finance {
    use super::*;

    pub fn initialize_stream(
        ctx: Context<InitializeStream>,
        stream_id: u64,
        total_deposit: u64,
        rate_per_second: u64,
        start_time: i64,
        stop_time: i64,
        cliff_time: i64,
    ) -> Result<()> {
        require_gt!(total_deposit, 0, SpoutError::ZeroDeposit);
        require_gt!(rate_per_second, 0, SpoutError::ZeroRate);
        require_gt!(stop_time, start_time, SpoutError::InvalidStreamTimeWindow);
        require!(cliff_time <= stop_time, SpoutError::InvalidCliffTime);

        let stream_duration = (stop_time - start_time) as u64;
        let expected_deposit = stream_duration
            .checked_mul(rate_per_second)
            .ok_or(SpoutError::MathOverflow)?;

        require_eq!(total_deposit, expected_deposit, SpoutError::DepositMismatch);

        let stream = &mut ctx.accounts.stream_account;
        stream.bump = ctx.bumps.stream_account;
        stream.vault_bump = ctx.bumps.escrow_vault;
        stream.sender = ctx.accounts.sender.key();
        stream.recipient = ctx.accounts.recipient.key();
        stream.mint = ctx.accounts.mint.key();
        stream.stream_id = stream_id;
        stream.total_deposit = total_deposit;
        stream.rate_per_second = rate_per_second;
        stream.start_time = start_time;
        stream.stop_time = stop_time;
        stream.cliff_time = cliff_time;
        stream.last_settlement_time = start_time;
        stream.withdrawn_amount = 0;
        stream.is_paused = false;
        stream.paused_at = 0;
        stream.total_paused_duration = 0;

        // Transfer funds from sender to escrow PDA vault
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.sender_token_account.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.sender.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token_interface::transfer_checked(cpi_ctx, total_deposit, ctx.accounts.mint.decimals)?;

        emit!(StreamCreatedEvent {
            stream_id,
            sender: stream.sender,
            recipient: stream.recipient,
            total_deposit,
            rate_per_second,
            start_time,
            stop_time,
        });

        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        let stream = &mut ctx.accounts.stream_account;
        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        require!(!stream.is_paused, SpoutError::StreamIsPaused);
        require!(current_time >= stream.cliff_time, SpoutError::CliffNotReached);

        let effective_time = std::cmp::min(current_time, stream.stop_time);
        let eligible_elapsed = effective_time
            .saturating_sub(stream.start_time)
            .saturating_sub(stream.total_paused_duration as i64);

        let total_vested = (eligible_elapsed as u64)
            .checked_mul(stream.rate_per_second)
            .ok_or(SpoutError::MathOverflow)?;

        let claimable = total_vested
            .min(stream.total_deposit)
            .saturating_sub(stream.withdrawn_amount);

        require_gt!(claimable, 0, SpoutError::NoClaimableFunds);

        stream.withdrawn_amount = stream
            .withdrawn_amount
            .checked_add(claimable)
            .ok_or(SpoutError::MathOverflow)?;
        stream.last_settlement_time = current_time;

        // Perform PDA Signer CPI Transfer to Recipient
        let stream_key = stream.key();
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"vault",
            stream_key.as_ref(),
            &[stream.vault_bump],
        ]];

        let cpi_accounts = TransferChecked {
            from: ctx.accounts.escrow_vault.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.recipient_token_account.to_account_info(),
            authority: ctx.accounts.escrow_vault.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
        );
        token_interface::transfer_checked(cpi_ctx, claimable, ctx.accounts.mint.decimals)?;

        emit!(StreamWithdrawnEvent {
            stream_id: stream.stream_id,
            recipient: stream.recipient,
            amount: claimable,
            total_withdrawn: stream.withdrawn_amount,
        });

        Ok(())
    }

    pub fn pause_stream(ctx: Context<ManageStream>) -> Result<()> {
        let stream = &mut ctx.accounts.stream_account;
        require!(!stream.is_paused, SpoutError::AlreadyPaused);

        let current_time = Clock::get()?.unix_timestamp;
        require!(current_time < stream.stop_time, SpoutError::StreamAlreadyEnded);

        stream.is_paused = true;
        stream.paused_at = current_time;

        emit!(StreamPausedEvent {
            stream_id: stream.stream_id,
            paused_at: current_time,
        });

        Ok(())
    }

    pub fn resume_stream(ctx: Context<ManageStream>) -> Result<()> {
        let stream = &mut ctx.accounts.stream_account;
        require!(stream.is_paused, SpoutError::NotPaused);

        let current_time = Clock::get()?.unix_timestamp;
        let paused_interval = current_time
            .checked_sub(stream.paused_at)
            .ok_or(SpoutError::MathOverflow)?;

        stream.total_paused_duration = stream
            .total_paused_duration
            .checked_add(paused_interval as u64)
            .ok_or(SpoutError::MathOverflow)?;

        stream.is_paused = false;
        stream.paused_at = 0;

        emit!(StreamResumedEvent {
            stream_id: stream.stream_id,
            resumed_at: current_time,
            additional_paused_seconds: paused_interval as u64,
        });

        Ok(())
    }

    pub fn cancel_stream(ctx: Context<CancelStream>) -> Result<()> {
        let stream = &ctx.accounts.stream_account;
        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        let effective_time = if stream.is_paused {
            stream.paused_at
        } else {
            std::cmp::min(current_time, stream.stop_time)
        };

        let eligible_elapsed = effective_time
            .saturating_sub(stream.start_time)
            .saturating_sub(stream.total_paused_duration as i64);

        let total_vested = (eligible_elapsed as u64)
            .checked_mul(stream.rate_per_second)
            .ok_or(SpoutError::MathOverflow)?;

        let recipient_claimable = total_vested
            .min(stream.total_deposit)
            .saturating_sub(stream.withdrawn_amount);

        let unvested_refund = stream
            .total_deposit
            .saturating_sub(stream.withdrawn_amount)
            .saturating_sub(recipient_claimable);

        let stream_key = stream.key();
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"vault",
            stream_key.as_ref(),
            &[stream.vault_bump],
        ]];

        // 1. Transfer earned/claimable portion to Recipient
        if recipient_claimable > 0 {
            let cpi_accounts_recipient = TransferChecked {
                from: ctx.accounts.escrow_vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.recipient_token_account.to_account_info(),
                authority: ctx.accounts.escrow_vault.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts_recipient,
                signer_seeds,
            );
            token_interface::transfer_checked(cpi_ctx, recipient_claimable, ctx.accounts.mint.decimals)?;
        }

        // 2. Refund unvested capital to Sender
        if unvested_refund > 0 {
            let cpi_accounts_sender = TransferChecked {
                from: ctx.accounts.escrow_vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.sender_token_account.to_account_info(),
                authority: ctx.accounts.escrow_vault.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts_sender,
                signer_seeds,
            );
            token_interface::transfer_checked(cpi_ctx, unvested_refund, ctx.accounts.mint.decimals)?;
        }

        emit!(StreamCanceledEvent {
            stream_id: stream.stream_id,
            recipient_amount: recipient_claimable,
            sender_refund: unvested_refund,
        });

        // Escrow vault will be closed via Anchor close constraint if token balance reaches 0.
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(stream_id: u64)]
pub struct InitializeStream<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,
    /// CHECK: Recipient is verified by account storage only.
    pub recipient: AccountInfo<'info>,
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        init,
        payer = sender,
        space = 8 + StreamAccount::INIT_SPACE,
        seeds = [b"stream", sender.key().as_ref(), recipient.key().as_ref(), &stream_id.to_le_bytes()],
        bump
    )]
    pub stream_account: Account<'info, StreamAccount>,

    #[account(
        init,
        payer = sender,
        seeds = [b"vault", stream_account.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = escrow_vault,
        token::token_program = token_program
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = sender
    )]
    pub sender_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub recipient: Signer<'info>,

    #[account(
        mut,
        seeds = [b"stream", stream_account.sender.as_ref(), recipient.key().as_ref(), &stream_account.stream_id.to_le_bytes()],
        bump = stream_account.bump,
        has_one = recipient @ SpoutError::InvalidRecipient,
        has_one = mint @ SpoutError::InvalidMint
    )]
    pub stream_account: Account<'info, StreamAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"vault", stream_account.key().as_ref()],
        bump = stream_account.vault_bump
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = recipient
    )]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ManageStream<'info> {
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"stream", stream_account.sender.as_ref(), stream_account.recipient.as_ref(), &stream_account.stream_id.to_le_bytes()],
        bump = stream_account.bump,
        constraint = stream_account.sender == authority.key() || stream_account.recipient == authority.key() @ SpoutError::Unauthorized
    )]
    pub stream_account: Account<'info, StreamAccount>,
}

#[derive(Accounts)]
pub struct CancelStream<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,

    #[account(
        mut,
        seeds = [b"stream", sender.key().as_ref(), stream_account.recipient.as_ref(), &stream_account.stream_id.to_le_bytes()],
        bump = stream_account.bump,
        has_one = sender @ SpoutError::Unauthorized,
        has_one = mint @ SpoutError::InvalidMint,
        close = sender
    )]
    pub stream_account: Account<'info, StreamAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        seeds = [b"vault", stream_account.key().as_ref()],
        bump = stream_account.vault_bump
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = sender
    )]
    pub sender_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint,
        token::authority = stream_account.recipient
    )]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

// -----------------------------------------------------------------------------
// STATE DEFINITION
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct StreamAccount {
    pub bump: u8,
    pub vault_bump: u8,
    pub sender: Pubkey,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub stream_id: u64,
    pub total_deposit: u64,
    pub rate_per_second: u64,
    pub start_time: i64,
    pub stop_time: i64,
    pub cliff_time: i64,
    pub last_settlement_time: i64,
    pub withdrawn_amount: u64,
    pub is_paused: bool,
    pub paused_at: i64,
    pub total_paused_duration: u64,
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct StreamCreatedEvent {
    pub stream_id: u64,
    pub sender: Pubkey,
    pub recipient: Pubkey,
    pub total_deposit: u64,
    pub rate_per_second: u64,
    pub start_time: i64,
    pub stop_time: i64,
}

#[event]
pub struct StreamWithdrawnEvent {
    pub stream_id: u64,
    pub recipient: Pubkey,
    pub amount: u64,
    pub total_withdrawn: u64,
}

#[event]
pub struct StreamPausedEvent {
    pub stream_id: u64,
    pub paused_at: i64,
}

#[event]
pub struct StreamResumedEvent {
    pub stream_id: u64,
    pub resumed_at: i64,
    pub additional_paused_seconds: u64,
}

#[event]
pub struct StreamCanceledEvent {
    pub stream_id: u64,
    pub recipient_amount: u64,
    pub sender_refund: u64,
}

#[error_code]
pub enum SpoutError {
    #[msg("Initial stream deposit must be greater than zero.")]
    ZeroDeposit,
    #[msg("Flow rate must be greater than zero tokens per second.")]
    ZeroRate,
    #[msg("Invalid stream duration: stop time must be strictly after start time.")]
    InvalidStreamTimeWindow,
    #[msg("Cliff timestamp cannot exceed stream completion timestamp.")]
    InvalidCliffTime,
    #[msg("Stream deposit amount does not equal duration * rate.")]
    DepositMismatch,
    #[msg("Arithmetic overflow in rate or balance calculation.")]
    MathOverflow,
    #[msg("Cliff has not yet elapsed; no tokens are available for withdrawal.")]
    CliffNotReached,
    #[msg("Stream is currently in a paused state.")]
    StreamIsPaused,
    #[msg("No withdrawable balance currently available.")]
    NoClaimableFunds,
    #[msg("Stream is already paused.")]
    AlreadyPaused,
    #[msg("Stream is not paused.")]
    NotPaused,
    #[msg("Stream timestamp has elapsed.")]
    StreamAlreadyEnded,
    #[msg("Unauthorized signer for this stream operation.")]
    Unauthorized,
    #[msg("Invalid recipient token account provided.")]
    InvalidRecipient,
    #[msg("Token mint mismatch.")]
    InvalidMint,
}