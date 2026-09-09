use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("Spout111111111111111111111111111111111111111");

pub const MAX_BATCH_SIZE: usize = 16;
pub const MAX_ORACLE_STALENESS_SECONDS: u64 = 30;
pub const BASIS_POINTS_DIVISOR: u64 = 10_000;

#[program]
pub mod hyperspout_engine {
    use super::*;

    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        market_id: u64,
        fee_bps: u16,
        oracle_price_feed: Pubkey,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        market.authority = ctx.accounts.authority.key();
        market.market_id = market_id;
        market.fee_bps = fee_bps;
        market.oracle_price_feed = oracle_price_feed;
        market.base_mint = ctx.accounts.base_mint.key();
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.bump = ctx.bumps.market;
        market.is_paused = false;
        Ok(())
    }

    pub fn deposit_escrow(ctx: Context<DepositEscrow>, amount: u64) -> Result<()> {
        require!(amount > 0, HyperSpoutError::ZeroDepositAmount);

        let escrow = &mut ctx.accounts.maker_escrow;
        if escrow.owner == Pubkey::default() {
            escrow.owner = ctx.accounts.maker.key();
            escrow.market = ctx.accounts.market.key();
            escrow.bump = ctx.bumps.maker_escrow;
            escrow.active_orders_bitmask = 0;
            escrow.deposited_balance = 0;
        }

        escrow.deposited_balance = escrow
            .deposited_balance
            .checked_add(amount)
            .ok_or(HyperSpoutError::MathOverflow)?;

        // Execute CPI to SPL Token Program
        let cpi_accounts = Transfer {
            from: ctx.accounts.maker_token_account.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.maker.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        emit!(EscrowDeposited {
            maker: ctx.accounts.maker.key(),
            market: ctx.accounts.market.key(),
            amount,
        });

        Ok(())
    }

    pub fn bitmask_cancel(ctx: Context<CancelOrder>, order_index: u8) -> Result<()> {
        require!(order_index < 64, HyperSpoutError::InvalidOrderIndex);
        let escrow = &mut ctx.accounts.maker_escrow;
        
        let mask = 1u64 << order_index;
        require!((escrow.active_orders_bitmask & mask) != 0, HyperSpoutError::OrderAlreadyCancelled);

        // Clear bitmask flag
        escrow.active_orders_bitmask &= !mask;

        emit!(OrderCancelled {
            maker: ctx.accounts.maker.key(),
            order_index,
        });

        Ok(())
    }

    pub fn batch_settle_orders<'info>(
        ctx: Context<'_, '_, '_, 'info, BatchSettle<'info>>,
        fills: Vec<OrderFillManifest>,
    ) -> Result<()> {
        let market = &ctx.accounts.market;
        require!(!market.is_paused, HyperSpoutError::MarketPaused);
        require!(fills.len() <= MAX_BATCH_SIZE, HyperSpoutError::BatchSizeExceeded);
        require!(!fills.is_empty(), HyperSpoutError::EmptyBatch);

        // 1. Validate Oracle Price Freshness
        let clock = Clock::get()?;
        let oracle_account_info = &ctx.accounts.oracle_account;
        require_keys_eq!(oracle_account_info.key(), market.oracle_price_feed, HyperSpoutError::InvalidOracleAccount);
        
        let price_feed = load_price_feed_from_account_info(oracle_account_info)
            .map_err(|_| HyperSpoutError::OracleParseFailure)?;
        let current_price = price_feed
            .get_price_no_older_than(clock.unix_timestamp, MAX_ORACLE_STALENESS_SECONDS)
            .ok_or(HyperSpoutError::OraclePriceStale)?;

        let oracle_price = current_price.price as u64;

        // 2. Iterate and Execute Multi-Leg Match Engine
        let market_id = market.market_id;
        let bump = market.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[b"market", market_id.to_le_bytes().as_ref(), &[bump]]];

        for fill in fills.iter() {
            // Price corridor safety assertion against oracle: Max 5% deviation allowed
            let min_allowed_price = oracle_price.saturating_mul(9500) / BASIS_POINTS_DIVISOR;
            let max_allowed_price = oracle_price.saturating_mul(10500) / BASIS_POINTS_DIVISOR;
            require!(
                fill.execution_price >= min_allowed_price && fill.execution_price <= max_allowed_price,
                HyperSpoutError::PriceSlippageExceeded
            );

            // Compute protocol fees and maker/taker amounts
            let gross_amount = fill.base_amount
                .checked_mul(fill.execution_price)
                .ok_or(HyperSpoutError::MathOverflow)?
                / 1_000_000; // Base decimals scale correction

            let protocol_fee = gross_amount
                .checked_mul(market.fee_bps as u64)
                .ok_or(HyperSpoutError::MathOverflow)?
                / BASIS_POINTS_DIVISOR;

            let net_maker_receive = gross_amount
                .checked_sub(protocol_fee)
                .ok_or(HyperSpoutError::MathOverflow)?;

            // Transfer Quote Asset: Taker -> Maker Escrow PDA Vault
            let transfer_taker_to_maker = Transfer {
                from: ctx.accounts.taker_quote_vault.to_account_info(),
                to: ctx.accounts.maker_quote_vault.to_account_info(),
                authority: ctx.accounts.taker.to_account_info(),
            };
            token::transfer(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), transfer_taker_to_maker),
                net_maker_receive,
            )?;

            // Transfer Base Asset: Maker PDA Escrow Vault -> Taker ATA
            let transfer_maker_to_taker = Transfer {
                from: ctx.accounts.market_base_vault.to_account_info(),
                to: ctx.accounts.taker_base_vault.to_account_info(),
                authority: ctx.accounts.market.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    transfer_maker_to_taker,
                    signer_seeds,
                ),
                fill.base_amount,
            )?;
        }

        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// Account Contexts
// -------------------------------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct InitializeMarket<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + MarketConfig::LEN,
        seeds = [b"market", market_id.to_le_bytes().as_ref()],
        bump
    )]
    pub market: Account<'info, MarketConfig>,
    pub base_mint: Account<'info, token::Mint>,
    pub quote_mint: Account<'info, token::Mint>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositEscrow<'info> {
    #[account(
        init_if_needed,
        payer = maker,
        space = 8 + MakerEscrow::LEN,
        seeds = [b"escrow", maker.key().as_ref(), market.key().as_ref()],
        bump
    )]
    pub maker_escrow: Account<'info, MakerEscrow>,
    #[account(mut)]
    pub market: Account<'info, MarketConfig>,
    #[account(
        mut,
        constraint = maker_token_account.owner == maker.key(),
        constraint = maker_token_account.mint == market.base_mint
    )]
    pub maker_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = escrow_vault.mint == market.base_mint
    )]
    pub escrow_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub maker: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelOrder<'info> {
    #[account(
        mut,
        has_one = owner @ HyperSpoutError::UnauthorizedSigner,
        seeds = [b"escrow", owner.key().as_ref(), market.key().as_ref()],
        bump = maker_escrow.bump
    )]
    pub maker_escrow: Account<'info, MakerEscrow>,
    pub market: Account<'info, MarketConfig>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
pub struct BatchSettle<'info> {
    #[account(
        mut,
        seeds = [b"market", market.market_id.to_le_bytes().as_ref()],
        bump = market.bump
    )]
    pub market: Account<'info, MarketConfig>,
    /// CHECK: Validated dynamically against market.oracle_price_feed
    pub oracle_account: AccountInfo<'info>,
    #[account(mut)]
    pub taker: Signer<'info>,
    #[account(
        mut,
        constraint = taker_quote_vault.owner == taker.key(),
        constraint = taker_quote_vault.mint == market.quote_mint
    )]
    pub taker_quote_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = taker_base_vault.owner == taker.key(),
        constraint = taker_base_vault.mint == market.base_mint
    )]
    pub taker_base_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = market_base_vault.mint == market.base_mint
    )]
    pub market_base_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = maker_quote_vault.mint == market.quote_mint
    )]
    pub maker_quote_vault: Account<'info, TokenAccount>,
    pub keeper: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

// -------------------------------------------------------------------------------------------------
// State & Data Structs
// -------------------------------------------------------------------------------------------------

#[account]
pub struct MarketConfig {
    pub authority: Pubkey,
    pub market_id: u64,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub oracle_price_feed: Pubkey,
    pub fee_bps: u16,
    pub bump: u8,
    pub is_paused: bool,
}

impl MarketConfig {
    pub const LEN: usize = 32 + 8 + 32 + 32 + 32 + 2 + 1 + 1;
}

#[account]
pub struct MakerEscrow {
    pub owner: Pubkey,
    pub market: Pubkey,
    pub deposited_balance: u64,
    pub active_orders_bitmask: u64,
    pub bump: u8,
}

impl MakerEscrow {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct OrderFillManifest {
    pub maker: Pubkey,
    pub order_index: u8,
    pub base_amount: u64,
    pub execution_price: u64,
}

// -------------------------------------------------------------------------------------------------
// Events & Errors
// -------------------------------------------------------------------------------------------------

#[event]
pub struct EscrowDeposited {
    pub maker: Pubkey,
    pub market: Pubkey,
    pub amount: u64,
}

#[event]
pub struct OrderCancelled {
    pub maker: Pubkey,
    pub order_index: u8,
}

#[error_code]
pub enum HyperSpoutError {
    #[msg("Amount must be greater than zero.")]
    ZeroDepositAmount,
    #[msg("Math arithmetic overflow.")]
    MathOverflow,
    #[msg("Order index out of range (0-63).")]
    InvalidOrderIndex,
    #[msg("Order already marked cancelled or filled.")]
    OrderAlreadyCancelled,
    #[msg("Market trading is currently paused.")]
    MarketPaused,
    #[msg("Batch size exceeds system maximum (16).")]
    BatchSizeExceeded,
    #[msg("Batch payload is empty.")]
    EmptyBatch,
    #[msg("Supplied oracle account address does not match market configuration.")]
    InvalidOracleAccount,
    #[msg("Failed to parse Pyth oracle data.")]
    OracleParseFailure,
    #[msg("Pyth oracle price feed is stale or confidence threshold unverified.")]
    OraclePriceStale,
    #[msg("Execution price violates oracle deviation safety corridors (Max 5%).")]
    PriceSlippageExceeded,
    #[msg("Unauthorized signer for escrow operation.")]
    UnauthorizedSigner,
}