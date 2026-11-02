use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer, MintTo, Burn};

declare_id!("Spout111111111111111111111111111111111111111");

// Math Scaling Constants: 64-bit fixed point precision scaling factor
const PRECISION_FACTOR: u128 = 1_000_000_000_000; // 1e12
const SECONDS_PER_YEAR: u128 = 31_536_000;
const MIN_SENIOR_COVERAGE_BPS: u16 = 12_000; // 120.00% minimum coverage ratio

#[program]
pub mod spout_engine {
    use super::*;

    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        pool_id: u64,
        senior_fixed_rate_bps: u16, // e.g., 800 = 8.00%
        performance_fee_bps: u16,
    ) -> Result<()> {
        require!(senior_fixed_rate_bps <= 3000, SpoutError::InvalidInterestRate); // Cap at 30%
        require!(performance_fee_bps <= 1000, SpoutError::InvalidFeeParameters);

        let pool = &mut ctx.accounts.pool_state;
        pool.admin = ctx.accounts.admin.key();
        pool.pool_id = pool_id;
        pool.underlying_mint = ctx.accounts.underlying_mint.key();
        pool.senior_mint = ctx.accounts.senior_mint.key();
        pool.junior_mint = ctx.accounts.junior_mint.key();
        pool.escrow_vault = ctx.accounts.escrow_vault.key();
        pool.senior_fixed_rate_bps = senior_fixed_rate_bps;
        pool.performance_fee_bps = performance_fee_bps;
        pool.last_settlement_timestamp = Clock::get()?.unix_timestamp;
        pool.total_senior_principal = 0;
        pool.total_junior_principal = 0;
        pool.accrued_senior_yield = 0;
        pool.unallocated_cash_drag = 0;
        pool.bump = ctx.bumps.pool_state;
        pool.escrow_bump = ctx.bumps.escrow_vault;
        pool.is_paused = false;
        pool.reentrancy_lock = false;

        emit!(PoolInitializedEvent {
            pool_id,
            admin: pool.admin,
            timestamp: pool.last_settlement_timestamp,
        });

        Ok(())
    }

    pub fn deposit_senior(ctx: Context<DepositSenior>, amount: u64) -> Result<()> {
        require!(amount > 0, SpoutError::ZeroDepositAmount);
        let pool = &mut ctx.accounts.pool_state;
        require!(!pool.is_paused, SpoutError::ProtocolPaused);
        require!(!pool.reentrancy_lock, SpoutError::ReentrancyDetected);
        pool.reentrancy_lock = true;

        // Lazy settlement of accrued yield up to current timestamp
        settle_internal_waterfall(pool, Clock::get()?.unix_timestamp)?;

        // Transfer underlying assets from depositor to program escrow vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.depositor_token_account.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.depositor.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        // Mint exact 1:1 spSEN tranche receipt tokens
        let seeds = &[
            b"spout_pool".as_ref(),
            &pool.pool_id.to_le_bytes(),
            &[pool.bump],
        ];
        let signer = &[&seeds[..]];
        let mint_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.senior_mint.to_account_info(),
                to: ctx.accounts.depositor_senior_receipt.to_account_info(),
                authority: pool.to_account_info(),
            },
            signer,
        );
        token::mint_to(mint_ctx, amount)?;

        pool.total_senior_principal = pool
            .total_senior_principal
            .checked_add(amount)
            .ok_or(SpoutError::MathOverflow)?;

        pool.reentrancy_lock = false;
        Ok(())
    }

    pub fn deposit_junior(ctx: Context<DepositJunior>, amount: u64) -> Result<()> {
        require!(amount > 0, SpoutError::ZeroDepositAmount);
        let pool = &mut ctx.accounts.pool_state;
        require!(!pool.is_paused, SpoutError::ProtocolPaused);
        require!(!pool.reentrancy_lock, SpoutError::ReentrancyDetected);
        pool.reentrancy_lock = true;

        settle_internal_waterfall(pool, Clock::get()?.unix_timestamp)?;

        let cpi_accounts = Transfer {
            from: ctx.accounts.depositor_token_account.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.depositor.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        let seeds = &[
            b"spout_pool".as_ref(),
            &pool.pool_id.to_le_bytes(),
            &[pool.bump],
        ];
        let signer = &[&seeds[..]];
        let mint_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.junior_mint.to_account_info(),
                to: ctx.accounts.depositor_junior_receipt.to_account_info(),
                authority: pool.to_account_info(),
            },
            signer,
        );
        token::mint_to(mint_ctx, amount)?;

        pool.total_junior_principal = pool
            .total_junior_principal
            .checked_add(amount)
            .ok_or(SpoutError::MathOverflow)?;

        pool.reentrancy_lock = false;
        Ok(())
    }

    pub fn initialize_stream(
        ctx: Context<InitializeStream>,
        flow_rate_per_sec: u64,
        duration_seconds: i64,
    ) -> Result<()> {
        require!(flow_rate_per_sec > 0, SpoutError::InvalidFlowRate);
        require!(duration_seconds > 0, SpoutError::InvalidDuration);

        let stream = &mut ctx.accounts.stream_account;
        let clock = Clock::get()?;

        stream.borrower = ctx.accounts.borrower.key();
        stream.pool_id = ctx.accounts.pool_state.pool_id;
        stream.flow_rate_per_sec = flow_rate_per_sec;
        stream.start_time = clock.unix_timestamp;
        stream.end_time = clock.unix_timestamp.checked_add(duration_seconds).ok_or(SpoutError::MathOverflow)?;
        stream.last_amortized_time = clock.unix_timestamp;
        stream.total_debt_drawn = (flow_rate_per_sec as u128)
            .checked_mul(duration_seconds as u128)
            .ok_or(SpoutError::MathOverflow)? as u64;
        stream.total_debt_settled = 0;
        stream.bump = ctx.bumps.stream_account;

        Ok(())
    }

    pub fn stream_repay_continuous(ctx: Context<StreamRepayContinuous>, amount: u64) -> Result<()> {
        require!(amount > 0, SpoutError::ZeroRepayAmount);
        let stream = &mut ctx.accounts.stream_account;
        let pool = &mut ctx.accounts.pool_state;

        require!(!pool.reentrancy_lock, SpoutError::ReentrancyDetected);
        pool.reentrancy_lock = true;

        let current_time = Clock::get()?.unix_timestamp;
        settle_internal_waterfall(pool, current_time)?;

        // Transfer continuous repayment tokens into the escrow vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.borrower_token_account.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.borrower.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        stream.total_debt_settled = stream
            .total_debt_settled
            .checked_add(amount)
            .ok_or(SpoutError::MathOverflow)?;
        stream.last_amortized_time = current_time;

        // Allocate directly to unallocated liquidity pool for tranche distributions
        pool.unallocated_cash_drag = pool
            .unallocated_cash_drag
            .checked_add(amount)
            .ok_or(SpoutError::MathOverflow)?;

        pool.reentrancy_lock = false;
        Ok(())
    }

    pub fn withdraw_senior(ctx: Context<WithdrawSenior>, shares_to_burn: u64) -> Result<()> {
        require!(shares_to_burn > 0, SpoutError::ZeroWithdrawAmount);
        let pool = &mut ctx.accounts.pool_state;
        require!(!pool.reentrancy_lock, SpoutError::ReentrancyDetected);
        pool.reentrancy_lock = true;

        settle_internal_waterfall(pool, Clock::get()?.unix_timestamp)?;

        // Compute pro-rata share of senior capital + fixed yield
        let senior_total = pool.total_senior_principal
            .checked_add(pool.accrued_senior_yield)
            .ok_or(SpoutError::MathOverflow)?;
        let senior_mint_supply = ctx.accounts.senior_mint.supply;
        require!(senior_mint_supply > 0, SpoutError::DivisionByZero);

        let payout_amount = ((shares_to_burn as u128)
            .checked_mul(senior_total as u128)
            .ok_or(SpoutError::MathOverflow)?)
            .checked_div(senior_mint_supply as u128)
            .ok_or(SpoutError::MathOverflow)? as u64;

        // Burn recipient LP tokens
        let burn_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.senior_mint.to_account_info(),
                from: ctx.accounts.depositor_senior_receipt.to_account_info(),
                authority: ctx.accounts.depositor.to_account_info(),
            },
        );
        token::burn(burn_ctx, shares_to_burn)?;

        // Transfer payout from escrow
        let seeds = &[
            b"spout_pool".as_ref(),
            &pool.pool_id.to_le_bytes(),
            &[pool.bump],
        ];
        let signer = &[&seeds[..]];

        let cpi_transfer = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.escrow_vault.to_account_info(),
                to: ctx.accounts.depositor_token_account.to_account_info(),
                authority: pool.to_account_info(),
            },
            signer,
        );
        token::transfer(cpi_transfer, payout_amount)?;

        // Adjust pool state
        pool.total_senior_principal = pool.total_senior_principal.saturating_sub(payout_amount);

        pool.reentrancy_lock = false;
        Ok(())
    }
}

// --------------------------------------------------------------------------------
// INTERNAL LAZY-EVALUATION SETTLEMENT ENGINE
// --------------------------------------------------------------------------------
fn settle_internal_waterfall(pool: &mut Account<PoolState>, current_time: i64) -> Result<()> {
    if current_time <= pool.last_settlement_timestamp {
        return Ok(());
    }

    let time_delta = (current_time - pool.last_settlement_timestamp) as u128;
    if time_delta == 0 || pool.total_senior_principal == 0 {
        pool.last_settlement_timestamp = current_time;
        return Ok(());
    }

    // Senior yield computation: (Principal * Rate * Delta) / (YEAR * 10000)
    let rate_scaled = pool.senior_fixed_rate_bps as u128;
    let continuous_nominal_yield = (pool.total_senior_principal as u128)
        .checked_mul(rate_scaled)
        .ok_or(SpoutError::MathOverflow)?
        .checked_mul(time_delta)
        .ok_or(SpoutError::MathOverflow)?
        .checked_div(SECONDS_PER_YEAR * 10_000)
        .ok_or(SpoutError::MathOverflow)? as u64;

    // Satisfy senior yield from unallocated cash drag first
    let senior_yield_satisfied = std::cmp::min(continuous_nominal_yield, pool.unallocated_cash_drag);
    pool.accrued_senior_yield = pool
        .accrued_senior_yield
        .checked_add(senior_yield_satisfied)
        .ok_or(SpoutError::MathOverflow)?;
    pool.unallocated_cash_drag = pool
        .unallocated_cash_drag
        .checked_sub(senior_yield_satisfied)
        .ok_or(SpoutError::MathOverflow)?;

    // Minimum Senior Coverage Verification Guard:
    // (Escrow - Accrued Yield) / Senior Principal >= MIN_SENIOR_COVERAGE_BPS
    if pool.total_senior_principal > 0 {
        let total_liabilities = pool.total_senior_principal
            .checked_add(pool.accrued_senior_yield)
            .ok_or(SpoutError::MathOverflow)?;
        let total_assets = pool.total_senior_principal
            .checked_add(pool.total_junior_principal)
            .checked_add(pool.unallocated_cash_drag)
            .ok_or(SpoutError::MathOverflow)?;

        let current_cr_bps = (total_assets as u128)
            .checked_mul(10_000)
            .ok_or(SpoutError::MathOverflow)?
            .checked_div(total_liabilities as u128)
            .unwrap_or(0) as u16;

        require!(current_cr_bps >= MIN_SENIOR_COVERAGE_BPS, SpoutError::CoverageRatioBreached);
    }

    pool.last_settlement_timestamp = current_time;
    Ok(())
}

// --------------------------------------------------------------------------------
// ACCOUNT VALIDATION STRUCTS
// --------------------------------------------------------------------------------
#[derive(Accounts)]
#[instruction(pool_id: u64)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + PoolState::INIT_SPACE,
        seeds = [b"spout_pool", pool_id.to_le_bytes().as_ref()],
        bump
    )]
    pub pool_state: Account<'info, PoolState>,

    pub underlying_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        mint::decimals = underlying_mint.decimals,
        mint::authority = pool_state,
        seeds = [b"senior_mint", pool_id.to_le_bytes().as_ref()],
        bump
    )]
    pub senior_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        mint::decimals = underlying_mint.decimals,
        mint::authority = pool_state,
        seeds = [b"junior_mint", pool_id.to_le_bytes().as_ref()],
        bump
    )]
    pub junior_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        token::mint = underlying_mint,
        token::authority = pool_state,
        seeds = [b"escrow_vault", pool_id.to_le_bytes().as_ref()],
        bump
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositSenior<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [b"spout_pool", pool_state.pool_id.to_le_bytes().as_ref()],
        bump = pool_state.bump
    )]
    pub pool_state: Account<'info, PoolState>,

    #[account(
        mut,
        address = pool_state.senior_mint
    )]
    pub senior_mint: Account<'info, Mint>,

    #[account(
        mut,
        address = pool_state.escrow_vault
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_token_account.owner == depositor.key(),
        constraint = depositor_token_account.mint == pool_state.underlying_mint
    )]
    pub depositor_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_senior_receipt.owner == depositor.key(),
        constraint = depositor_senior_receipt.mint == pool_state.senior_mint
    )]
    pub depositor_senior_receipt: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct DepositJunior<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [b"spout_pool", pool_state.pool_id.to_le_bytes().as_ref()],
        bump = pool_state.bump
    )]
    pub pool_state: Account<'info, PoolState>,

    #[account(
        mut,
        address = pool_state.junior_mint
    )]
    pub junior_mint: Account<'info, Mint>,

    #[account(
        mut,
        address = pool_state.escrow_vault
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_token_account.owner == depositor.key(),
        constraint = depositor_token_account.mint == pool_state.underlying_mint
    )]
    pub depositor_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_junior_receipt.owner == depositor.key(),
        constraint = depositor_junior_receipt.mint == pool_state.junior_mint
    )]
    pub depositor_junior_receipt: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct InitializeStream<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,

    pub pool_state: Account<'info, PoolState>,

    #[account(
        init,
        payer = borrower,
        space = 8 + StreamLedger::INIT_SPACE,
        seeds = [b"stream", pool_state.pool_id.to_le_bytes().as_ref(), borrower.key().as_ref()],
        bump
    )]
    pub stream_account: Account<'info, StreamLedger>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct StreamRepayContinuous<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,

    #[account(
        mut,
        seeds = [b"spout_pool", pool_state.pool_id.to_le_bytes().as_ref()],
        bump = pool_state.bump
    )]
    pub pool_state: Account<'info, PoolState>,

    #[account(
        mut,
        seeds = [b"stream", pool_state.pool_id.to_le_bytes().as_ref(), borrower.key().as_ref()],
        bump = stream_account.bump,
        has_one = borrower
    )]
    pub stream_account: Account<'info, StreamLedger>,

    #[account(
        mut,
        address = pool_state.escrow_vault
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = borrower_token_account.owner == borrower.key(),
        constraint = borrower_token_account.mint == pool_state.underlying_mint
    )]
    pub borrower_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct WithdrawSenior<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [b"spout_pool", pool_state.pool_id.to_le_bytes().as_ref()],
        bump = pool_state.bump
    )]
    pub pool_state: Account<'info, PoolState>,

    #[account(
        mut,
        address = pool_state.senior_mint
    )]
    pub senior_mint: Account<'info, Mint>,

    #[account(
        mut,
        address = pool_state.escrow_vault
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_token_account.owner == depositor.key(),
        constraint = depositor_token_account.mint == pool_state.underlying_mint
    )]
    pub depositor_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_senior_receipt.owner == depositor.key(),
        constraint = depositor_senior_receipt.mint == pool_state.senior_mint
    )]
    pub depositor_senior_receipt: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

// --------------------------------------------------------------------------------
// STATE DEFINITIONS
// --------------------------------------------------------------------------------
#[account]
#[derive(InitSpace)]
pub struct PoolState {
    pub admin: Pubkey,
    pub pool_id: u64,
    pub underlying_mint: Pubkey,
    pub senior_mint: Pubkey,
    pub junior_mint: Pubkey,
    pub escrow_vault: Pubkey,
    pub senior_fixed_rate_bps: u16,
    pub performance_fee_bps: u16,
    pub last_settlement_timestamp: i64,
    pub total_senior_principal: u64,
    pub total_junior_principal: u64,
    pub accrued_senior_yield: u64,
    pub unallocated_cash_drag: u64,
    pub bump: u8,
    pub escrow_bump: u8,
    pub is_paused: bool,
    pub reentrancy_lock: bool,
}

#[account]
#[derive(InitSpace)]
pub struct StreamLedger {
    pub borrower: Pubkey,
    pub pool_id: u64,
    pub flow_rate_per_sec: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub last_amortized_time: i64,
    pub total_debt_drawn: u64,
    pub total_debt_settled: u64,
    pub bump: u8,
}

// --------------------------------------------------------------------------------
// EVENTS & ERRORS
// --------------------------------------------------------------------------------
#[event]
pub struct PoolInitializedEvent {
    pub pool_id: u64,
    pub admin: Pubkey,
    pub timestamp: i64,
}

#[error_code]
pub enum SpoutError {
    #[msg("Provided interest rate exceeds maximum protocol ceiling")]
    InvalidInterestRate,
    #[msg("Protocol fee parameters exceed maximum permitted bounds")]
    InvalidFeeParameters,
    #[msg("Deposit amount must be strictly greater than zero")]
    ZeroDepositAmount,
    #[msg("Repay amount must be strictly greater than zero")]
    ZeroRepayAmount,
    #[msg("Withdraw amount must be strictly greater than zero")]
    ZeroWithdrawAmount,
    #[msg("Protocol functions are temporarily paused by governance")]
    ProtocolPaused,
    #[msg("Reentrant call intercepted by security guard")]
    ReentrancyDetected,
    #[msg("Mathematical overflow encountered during calculation")]
    MathOverflow,
    #[msg("Continuous stream flow rate must exceed zero tokens per second")]
    InvalidFlowRate,
    #[msg("Stream duration must be positive")]
    InvalidDuration,
    #[msg("Division by zero in liquidity calculation")]
    DivisionByZero,
    #[msg("Senior tranche coverage ratio breached! Junior tranche locked")]
    CoverageRatioBreached,
}