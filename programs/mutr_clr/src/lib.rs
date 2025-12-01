use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer, MintTo};

declare_id!("CLRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR");

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
        state.stake_fee_bps = stake_fee_bps; // e.g. 300 = 3%
        state.unstake_fee_bps = unstake_fee_bps; // e.g. 300 = 3%
        state.lower_threshold = lower_threshold;
        state.upper_threshold = upper_threshold;
        state.acc_reward_per_share = 0;
        state.total_dividend_shares = 0;
        state.bump = *ctx.bumps.get("state").unwrap();
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
        let state_seeds: &[&[u8]] = &[
            b"state",
            &[state.bump],
        ];
        let signer_seeds = &[state_seeds];

        let cpi_accounts = MintTo {
            mint: ctx.accounts.xmutr_mint.to_account_info(),
            to: ctx.accounts.user_xmutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
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

    /// Unstake xMUTR and withdraw MUTR from the CLR (fee stays in CLR).
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
        let state_seeds: &[&[u8]] = &[
            b"state",
            &[state.bump],
        ];
        let signer_seeds = &[state_seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.user_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
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
        let state_seeds: &[&[u8]] = &[
            b"state",
            &[state.bump],
        ];
        let signer_seeds = &[state_seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.user_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
        );
        token::transfer(cpi_ctx, pending)?;

        Ok(())
    }

    /// Pay prize to a winner from the CLR vault (for approved games later).
    pub fn send_prize(ctx: Context<SendPrize>, amount: u64) -> Result<()> {
        require!(amount > 0, MutrError::InvalidAmount);

        let state = &ctx.accounts.state;
        let state_seeds: &[&[u8]] = &[
            b"state",
            &[state.bump],
        ];
        let signer_seeds = &[state_seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.clr_vault.to_account_info(),
            to: ctx.accounts.winner_mutr_account.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
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
// Data structures & error types
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

impl GlobalState {
    pub const LEN: usize = 32  // authority
        + 32 // mutr_mint
        + 32 // xmutr_mint
        + 32 // clr_vault
        + 2  // stake_fee_bps
        + 2  // unstake_fee_bps
        + 8  // lower_threshold
        + 8  // upper_threshold
        + 16 // acc_reward_per_share
        + 16 // total_dividend_shares
        + 1; // bump
}

#[account]
pub struct UserState {
    pub owner: Pubkey,
    pub staked_shares: u64,
    pub dividend_shares: u64,
    pub reward_debt: u128,
    pub pending_rewards: u128,
}

impl UserState {
    pub const LEN: usize = 32 // owner
        + 8  // staked_shares
        + 8  // dividend_shares
        + 16 // reward_debt
        + 16; // pending_rewards
}

// -----------------------------------------------------------------------------
// Accounts
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeClr<'info> {
    #[account(
        init,
        payer = authority,
        seeds = [b"state"],
        bump,
        space = 8 + GlobalState::LEN
    )]
    pub state: Account<'info, GlobalState>,

    /// MUTR mint (existing SPL token mint)
    pub mutr_mint: Account<'info, Mint>,

    /// xMUTR liquidity share mint (must have mint authority set to `state` PDA)
    #[account(mut)]
    pub xmutr_mint: Account<'info, Mint>,

    /// CLR vault that holds MUTR, owned by `state` PDA
    #[account(
        mut,
        constraint = clr_vault.mint == mutr_mint.key() @ MutrError::InvalidMint,
        constraint = clr_vault.owner == state.key() @ MutrError::Unauthorized
    )]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        constraint = mutr_mint.key() == state.mutr_mint @ MutrError::InvalidMint
    )]
    pub mutr_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = xmutr_mint.key() == state.xmutr_mint @ MutrError::InvalidMint
    )]
    pub xmutr_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = clr_vault.key() == state.clr_vault @ MutrError::InvalidVault,
        constraint = clr_vault.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = clr_vault.owner == state.key() @ MutrError::Unauthorized
    )]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_mutr_account.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = user_mutr_account.owner == user.key() @ MutrError::Unauthorized
    )]
    pub user_mutr_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_xmutr_account.mint == state.xmutr_mint @ MutrError::InvalidMint,
        constraint = user_xmutr_account.owner == user.key() @ MutrError::Unauthorized
    )]
    pub user_xmutr_account: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + UserState::LEN,
        seeds = [b"user_state", user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        constraint = mutr_mint.key() == state.mutr_mint @ MutrError::InvalidMint
    )]
    pub mutr_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = xmutr_mint.key() == state.xmutr_mint @ MutrError::InvalidMint
    )]
    pub xmutr_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = clr_vault.key() == state.clr_vault @ MutrError::InvalidVault,
        constraint = clr_vault.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = clr_vault.owner == state.key() @ MutrError::Unauthorized
    )]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_mutr_account.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = user_mutr_account.owner == user.key() @ MutrError::Unauthorized
    )]
    pub user_mutr_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_xmutr_account.mint == state.xmutr_mint @ MutrError::InvalidMint,
        constraint = user_xmutr_account.owner == user.key() @ MutrError::Unauthorized
    )]
    pub user_xmutr_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"user_state", user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct JoinDividendPool<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        mut,
        seeds = [b"user_state", user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,

    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct LeaveDividendPool<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        mut,
        seeds = [b"user_state", user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,

    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct RecordProfit<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump,
        has_one = authority @ MutrError::Unauthorized
    )]
    pub state: Account<'info, GlobalState>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        mut,
        constraint = clr_vault.key() == state.clr_vault @ MutrError::InvalidVault,
        constraint = clr_vault.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = clr_vault.owner == state.key() @ MutrError::Unauthorized
    )]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_mutr_account.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = user_mutr_account.owner == user.key() @ MutrError::Unauthorized
    )]
    pub user_mutr_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"user_state", user.key().as_ref()],
        bump
    )]
    pub user_state: Account<'info, UserState>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SendPrize<'info> {
    #[account(
        mut,
        seeds = [b"state"],
        bump = state.bump
    )]
    pub state: Account<'info, GlobalState>,

    #[account(
        mut,
        constraint = clr_vault.key() == state.clr_vault @ MutrError::InvalidVault,
        constraint = clr_vault.mint == state.mutr_mint @ MutrError::InvalidMint,
        constraint = clr_vault.owner == state.key() @ MutrError::Unauthorized
    )]
    pub clr_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = winner_mutr_account.mint == state.mutr_mint @ MutrError::InvalidMint
    )]
    pub winner_mutr_account: Account<'info, TokenAccount>,

    /// Game authority; later restricted to approved games
    pub game: Signer<'info>,

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
    #[msg("Invalid mint")]
    InvalidMint,
    #[msg("Invalid CLR vault")]
    InvalidVault,
}


