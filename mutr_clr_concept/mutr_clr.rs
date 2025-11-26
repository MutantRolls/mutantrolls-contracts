// -----------------------------------------------------------------------------
// MutantRolls — MUTR CLR (Combined Liquidity Reserve)
// Solana / Anchor – Conceptual Rust Design 
// -----------------------------------------------------------------------------
// This file outlines the core design for the MutantRolls (MUTR) 
// Combined Liquidity Reserve (CLR) system. The CLR acts as the 
// central engine behind all MUTR game economics.
//
// It describes how MUTR can be staked to mint xMUTR share-tokens,
// how the CLR bankroll grows, how dividends are generated and paid
// to stakers, and how the vault handles game prizes and profits.
//
// The mechanics include:
//   • Staking MUTR → minting xMUTR shares (liquidity provider token)
//   • Unstaking xMUTR → withdrawing MUTR from the CLR vault
//   • A dividend pool where users can earn a share of CLR profits
//   • Automatic profit distribution using a reward-per-share model
//   • Game prize payouts directly from the CLR bankroll
// -----------------------------------------------------------------------------

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer, MintTo, Burn};

declare_id!("CLRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR"); // placeholder

/// Precision for reward accounting (like 1e12)
const REWARD_PRECISION: u128 = 1_000_000_000_000;

#[program]
pub mod mutr_clr {
    use super::*;

    /// One-time initializer. Creates global state and wires up mints/accounts.
    pub fn initialize_clr(
        ctx: Context<InitializeClr>,
        stake_fee_bps: u16,
        unstake_fee_bps: u16,
        lower_threshold: u64,
        upper_threshold: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.authority = ctx.accounts.authority.key();
        state.mutr_mint = ctx.accounts.mutr_mint.key();
        state.xmutr_mint = ctx.accounts.xmutr_mint.key();
        state.clr_vault = ctx.accounts.clr_vault.key();
        state.stake_fee_bps = stake_fee_bps;     // e.g. 300 = 3%
        state.unstake_fee_bps = unstake_fee_bps; // e.g. 300 = 3%
        state.lower_threshold = lower_threshold;
        state.upper_threshold = upper_threshold;
        state.acc_reward_per_share = 0;
        state.total_dividend_shares = 0;
        Ok(())
    }

    /// Stake MUTR into the CLR and mint xMUTR to the user.
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount > 0, MutrError::InvalidAmount);

        let state = &ctx.accounts.state;
        let clr_vault_before = ctx.accounts.clr_vault.amount;

        // 1) Transfer MUTR from user to CLR vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_mutr_account.to_account_info(),
            to: ctx.accounts.clr_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        // 2) Apply stake fee (fee stays inside CLR, so we only issue shares for net amount)
        let net_amount = apply_fee(amount, state.stake_fee_bps)?;

        // 3) Determine how many xMUTR to mint
        let xmutr_supply = ctx.accounts.xmutr_mint.supply;
        let shares_to_mint = if xmutr_supply == 0 || clr_vault_before == 0 {
            // First staker or empty vault: 1:1 (minus fee)
            net_amount
        } else {
            // shares = net_amount * total_shares / clr_balance_before
            (net_amount as u128)
                .checked_mul(xmutr_supply as u128)
                .unwrap()
                .checked_div(clr_vault_before as u128)
                .unwrap() as u64
        };

        require!(shares_to_mint > 0, MutrError::ZeroShares);

        // 4) Mint xMUTR to user (program as mint authority via PDA)
        let seeds = &[b"state".as_ref()];
        let signer = &[&seeds[..]];

        let cpi_accounts = MintTo {
            mint: ctx.accounts.xmutr_mint.to_account_info(),
            to: ctx.accounts.user_xmutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::mint_to(cpi_ctx, shares_to_mint)?;

        // 5) Update user state
        let user_state = &mut ctx.accounts.user_state;
        if user_state.owner == Pubkey::default() {
            user_state.owner = ctx.accounts.user.key();
        }
        require_keys_eq!(user_state.owner, ctx.accounts.user.key(), MutrError::Unauthorized);
        user_state.staked_shares = user_state
            .staked_shares
            .checked_add(shares_to_mint)
            .ok_or(MutrError::MathOverflow)?;

        Ok(())
    }

    /// Unstake xMUTR and withdraw MUTR from the CLR (3% fee stays in CLR).
    pub fn unstake(ctx: Context<Unstake>, shares: u64) -> Result<()> {
        require!(shares > 0, MutrError::InvalidAmount);

        let state = &ctx.accounts.state;
        let user_state = &mut ctx.accounts.user_state;
        require!(
            user_state.staked_shares >= shares + user_state.dividend_shares,
            MutrError::InsufficientShares
        );

        // 1) Burn xMUTR from user
        let cpi_accounts = Burn {
            mint: ctx.accounts.xmutr_mint.to_account_info(),
            from: ctx.accounts.user_xmutr_account.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::burn(cpi_ctx, shares)?;

        user_state.staked_shares = user_state
            .staked_shares
            .checked_sub(shares)
            .ok_or(MutrError::MathOverflow)?;

        // 2) Calculate how much MUTR this share amount is worth
        let clr_balance = ctx.accounts.clr_vault.amount;
        let xmutr_supply = ctx.accounts.xmutr_mint.supply;
        require!(xmutr_supply > 0, MutrError::ZeroShares);

        let mutt_before_fee = (clr_balance as u128)
            .checked_mul(shares as u128)
            .unwrap()
            .checked_div(xmutr_supply as u128)
            .unwrap() as u64;

        // 3) Apply unstake fee
        let net_amount = apply_fee(mutt_before_fee, state.unstake_fee_bps)?;

        // 4) Transfer MUTR from CLR vault to user
        let seeds = &[b"state".as_ref()];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.user_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, net_amount)?;

        Ok(())
    }

    /// Move xMUTR into the dividend pool (0% fee, but must settle rewards first).
    pub fn join_dividend_pool(ctx: Context<JoinDividendPool>, shares: u64) -> Result<()> {
        require!(shares > 0, MutrError::InvalidAmount);

        let state = &mut ctx.accounts.state;
        let user_state = &mut ctx.accounts.user_state;
        require!(user_state.staked_shares >= shares, MutrError::InsufficientShares);

        // settle current rewards
        settle_user_rewards(state, user_state)?;

        user_state.staked_shares = user_state
            .staked_shares
            .checked_sub(shares)
            .ok_or(MutrError::MathOverflow)?;
        user_state.dividend_shares = user_state
            .dividend_shares
            .checked_add(shares)
            .ok_or(MutrError::MathOverflow)?;

        state.total_dividend_shares = state
            .total_dividend_shares
            .checked_add(shares as u128)
            .ok_or(MutrError::MathOverflow)?;

        // update reward debt
        user_state.reward_debt = (user_state.dividend_shares as u128)
            .checked_mul(state.acc_reward_per_share)
            .unwrap();

        Ok(())
    }

    /// Leave the dividend pool (4% fee on shares, fee is burned).
    pub fn leave_dividend_pool(ctx: Context<LeaveDividendPool>, shares: u64) -> Result<()> {
        require!(shares > 0, MutrError::InvalidAmount);

        let state = &mut ctx.accounts.state;
        let user_state = &mut ctx.accounts.user_state;
        require!(user_state.dividend_shares >= shares, MutrError::InsufficientShares);

        // settle rewards first
        settle_user_rewards(state, user_state)?;

        // apply 4% exit fee on shares (burned)
        let fee_bps: u16 = 400;
        let net_shares = apply_fee(shares, fee_bps)?;

        // move net shares back to staked_shares
        user_state.dividend_shares = user_state
            .dividend_shares
            .checked_sub(shares)
            .ok_or(MutrError::MathOverflow)?;

        user_state.staked_shares = user_state
            .staked_shares
            .checked_add(net_shares)
            .ok_or(MutrError::MathOverflow)?;

        // update global dividend supply (we remove the full shares, including fee)
        state.total_dividend_shares = state
            .total_dividend_shares
            .checked_sub(shares as u128)
            .ok_or(MutrError::MathOverflow)?;

        // update reward debt
        user_state.reward_debt = (user_state.dividend_shares as u128)
            .checked_mul(state.acc_reward_per_share)
            .unwrap();

        Ok(())
    }

    /// Record new profit in the CLR and update reward per share.
    /// Simplified MasterChef-style accounting.
    pub fn record_profit(ctx: Context<RecordProfit>, profit_amount: u64) -> Result<()> {
        let state = &mut ctx.accounts.state;

        require!(state.total_dividend_shares > 0, MutrError::NoDividendShares);

        let profit_u128 = profit_amount as u128;
        let increment = profit_u128
            .checked_mul(REWARD_PRECISION)
            .unwrap()
            .checked_div(state.total_dividend_shares)
            .unwrap();

        state.acc_reward_per_share = state
            .acc_reward_per_share
            .checked_add(increment)
            .ok_or(MutrError::MathOverflow)?;

        Ok(())
    }

    /// Claim accumulated MUTR rewards from the dividend pool.
    pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let user_state = &mut ctx.accounts.user_state;

        let pending = pending_rewards(state, user_state)?;
        if pending == 0 {
            return Ok(());
        }

        // update accounting before transfer
        user_state.pending_rewards = 0;
        user_state.reward_debt = (user_state.dividend_shares as u128)
            .checked_mul(state.acc_reward_per_share)
            .unwrap();

        // transfer from CLR vault to user
        let seeds = &[b"state".as_ref()];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.user_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, pending)?;

        Ok(())
    }

    /// Pay prize to a winner from the CLR vault (for approved games later).
    pub fn send_prize(ctx: Context<SendPrize>, amount: u64) -> Result<()> {
        require!(amount > 0, MutrError::InvalidAmount);

        let seeds = &[b"state".as_ref()];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.winner_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, amount)?;

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Helper functions
// -----------------------------------------------------------------------------

/// Apply fee in basis points; fee is kept in CLR (we just return net).
fn apply_fee(amount: u64, fee_bps: u16) -> Result<u64> {
    let fee = (amount as u128)
        .checked_mul(fee_bps as u128)
        .unwrap()
        .checked_div(10_000)
        .unwrap() as u64;
    Ok(amount
        .checked_sub(fee)
        .ok_or(MutrError::MathOverflow)?)
}

/// Settle user rewards into pending_rewards.
fn settle_user_rewards(state: &GlobalState, user: &mut UserState) -> Result<()> {
    let pending = pending_rewards(state, user)?;
    user.pending_rewards = user
        .pending_rewards
        .checked_add(pending)
        .ok_or(MutrError::MathOverflow)?;
    Ok(())
}

/// Calculate pending rewards (current).
fn pending_rewards(state: &GlobalState, user: &UserState) -> Result<u64> {
    if user.dividend_shares == 0 {
        return Ok(user.pending_rewards as u64);
    }
    let acc_per_share = state.acc_reward_per_share;
    let accumulated = (user.dividend_shares as u128)
        .checked_mul(acc_per_share)
        .unwrap();
    let pending_u128 = accumulated
        .checked_sub(user.reward_debt)
        .unwrap()
        .checked_div(REWARD_PRECISION)
        .unwrap()
        .checked_add(user.pending_rewards)
        .unwrap();
    Ok(pending_u128 as u64)
}

// -----------------------------------------------------------------------------
// Data structures & error types (simplified / conceptual)
// -----------------------------------------------------------------------------

#[account]
pub struct GlobalState {
    pub authority: Pubkey,
    pub mutr_mint: Pubkey,
    pub xmutr_mint: Pubkey,
    pub clr_vault: Pubkey,

    pub stake_fee_bps: u16,
    pub unstake_fee_bps: u16,
    pub lower_threshold: u64,
    pub upper_threshold: u64,

    pub acc_reward_per_share: u128,
    pub total_dividend_shares: u128,

    pub bump: u8,
}

#[account]
pub struct UserState {
    pub owner: Pubkey,
    pub staked_shares: u64,
    pub dividend_shares: u64,
    pub reward_debt: u128,
    pub pending_rewards: u128,
}

// The Accounts structs below are high-level sketches; they would need
// proper constraints & PDA seeds before production use.

#[derive(Accounts)]
pub struct InitializeClr<'info> {
    #[account(init, payer = authority, space = 8 + 200)]
    pub state: Account<'info, GlobalState>,

    pub mutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub xmutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,

    #[account(mut)]
    pub mutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub xmutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_mutr_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_xmutr_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_state: Account<'info, UserState>,
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,

    #[account(mut)]
    pub mutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub xmutr_mint: Account<'info, Mint>,
    #[account(mut)]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_mutr_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_xmutr_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_state: Account<'info, UserState>,
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct JoinDividendPool<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_state: Account<'info, UserState>,
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct LeaveDividendPool<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_state: Account<'info, UserState>,
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct RecordProfit<'info> {
    #[account(mut, has_one = authority)]
    pub state: Account<'info, GlobalState>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut)]
    pub clr_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_mutr_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_state: Account<'info, UserState>,
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SendPrize<'info> {
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut)]
    pub clr_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub winner_mutr_account: Account<'info, TokenAccount>,
    pub game: Signer<'info>, // later: restrict to approved games
    pub token_program: Program<'info, Token>,
}

#[error_code]
pub enum MutrError {
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Zero shares")]
    ZeroShares,
    #[msg("Insufficient shares")]
    InsufficientShares,
    #[msg("No dividend shares")]
    NoDividendShares,
    #[msg("Unauthorized")]
    Unauthorized,
}
