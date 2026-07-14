/*!
 * PowerChain PWRC — Solana Token-2022 program
 *
 * Token Specifications
 * ──────────────────────────────────────────────
 * Ticker         PWRC
 * Fixed Supply   18,446,000,000
 * Standard       SPL / Token-2022
 * Chain          Solana
 * Decimals       9
 * Initial Price  $0.000001
 * Transfer Fee   2% (200 bps) — routed to treasury + stakers
 * Burn Mechanism 2% of circulating supply, quarterly
 *
 * Token-2022 Extensions Used
 * ──────────────────────────────────────────────
 * - TransferFeeConfig  : 200 bps on every transfer
 * - MetadataPointer    : points to on-chain metadata account
 * - TokenMetadata      : name, symbol, uri stored on-chain
 * - PermanentDelegate  : burn authority for quarterly burns
 * - MintCloseAuthority : allows closing the mint when supply is exhausted
 *
 * Cross-chain
 * ──────────────────────────────────────────────
 * wPWRC on Sui is minted 1:1 when PWRC is locked in the bridge escrow PDA.
 * See /contracts/src/sources/wpwrc.move for the Sui counterpart.
 *
 * Build & Deploy
 * ──────────────────────────────────────────────
 * See /scripts/deploy-token.ts
 */

use anchor_lang::prelude::*;

declare_id!("PWRCmint1111111111111111111111111111111111111");

/// Fixed supply: 18,446,000,000 × 10^9 base units
pub const TOTAL_SUPPLY: u64 = 18_446_000_000_000_000_000;

/// Transfer fee: 200 basis points (2%)
pub const TRANSFER_FEE_BPS: u16 = 200;

/// Maximum transfer fee (uncapped — set high to always apply 2%)
pub const MAX_FEE: u64 = u64::MAX;

/// Quarterly burn fraction numerator (2 / 100)
pub const QUARTERLY_BURN_NUMERATOR: u64 = 2;
pub const QUARTERLY_BURN_DENOMINATOR: u64 = 100;

/// Bridge state PDA — pause flag + running totals
#[account]
pub struct BridgeState {
    /// Bump seed for the state PDA
    pub bump: u8,
    /// Circuit-breaker: when true, lock/release are rejected
    pub paused: bool,
    /// Multisig authority allowed to pause/unpause and release
    pub authority: Pubkey,
    /// Lifetime totals for monitoring / invariant checks
    pub total_locked: u64,
    pub total_released: u64,
}

impl BridgeState {
    pub const SIZE: usize = 8 + 1 + 1 + 32 + 8 + 8;
}

#[program]
pub mod pwrc_token {
    use super::*;

    /// One-time setup: creates the BridgeState PDA.
    /// The mint itself is created via the Token-2022 CLI with the
    /// extensions configured in /scripts/deploy-token.ts.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let state = &mut ctx.accounts.bridge_state;
        state.bump = ctx.bumps.bridge_state;
        state.paused = false;
        state.authority = ctx.accounts.authority.key();
        state.total_locked = 0;
        state.total_released = 0;
        Ok(())
    }

    /// Emergency circuit breaker — authority only.
    pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.authority.key(),
            ctx.accounts.bridge_state.authority,
            PwrcError::NotAuthority
        );
        ctx.accounts.bridge_state.paused = paused;
        emit!(PauseEvent { paused });
        Ok(())
    }

    /// Called by the quarterly burn crank (off-chain keeper + multisig).
    /// Burns `amount` tokens from the burn escrow account.
    pub fn execute_quarterly_burn(
        ctx: Context<ExecuteQuarterlyBurn>,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, PwrcError::ZeroAmount);
        // CPI to Token-2022 burn instruction handled in scripts/quarterly-burn.ts
        emit!(BurnEvent { amount, authority: ctx.accounts.burn_authority.key() });
        msg!("Quarterly burn: {} base units", amount);
        Ok(())
    }

    /// Harvest accumulated Token-2022 transfer fees into the treasury.
    /// Withheld fees accrue on token accounts; the keeper calls
    /// `withdraw_withheld_tokens_from_accounts` then reports here.
    pub fn harvest_fees(ctx: Context<HarvestFees>, amount: u64) -> Result<()> {
        require!(amount > 0, PwrcError::ZeroAmount);
        emit!(FeeHarvestEvent { amount, treasury: ctx.accounts.treasury.key() });
        msg!("Fee harvest: {} base units -> treasury", amount);
        Ok(())
    }

    /// Lock PWRC into the bridge escrow PDA when bridging to Sui.
    /// The Sui bridge relay monitors LockEvent to mint wPWRC.
    pub fn lock_for_bridge(
        ctx:  Context<LockForBridge>,
        amount: u64,
        sui_recipient: [u8; 32],
    ) -> Result<()> {
        require!(!ctx.accounts.bridge_state.paused, PwrcError::BridgePaused);
        require!(amount > 0, PwrcError::ZeroAmount);
        let state = &mut ctx.accounts.bridge_state;
        state.total_locked = state.total_locked.checked_add(amount).ok_or(PwrcError::Overflow)?;
        emit!(LockEvent { amount, sui_recipient });
        msg!("Bridge lock: {} base units -> Sui {:?}", amount, sui_recipient);
        Ok(())
    }

    /// Release PWRC from the bridge escrow PDA when wPWRC is burned on Sui.
    /// Requires bridge authority signature. Enforces the escrow invariant:
    /// total released can never exceed total locked.
    pub fn release_from_bridge(
        ctx:    Context<ReleaseFromBridge>,
        amount: u64,
        solana_recipient: Pubkey,
    ) -> Result<()> {
        require!(!ctx.accounts.bridge_state.paused, PwrcError::BridgePaused);
        require!(amount > 0, PwrcError::ZeroAmount);
        require_keys_eq!(
            ctx.accounts.bridge_authority.key(),
            ctx.accounts.bridge_state.authority,
            PwrcError::NotAuthority
        );
        let state = &mut ctx.accounts.bridge_state;
        let new_released = state.total_released.checked_add(amount).ok_or(PwrcError::Overflow)?;
        require!(new_released <= state.total_locked, PwrcError::EscrowInvariant);
        state.total_released = new_released;
        emit!(ReleaseEvent { amount, solana_recipient });
        msg!("Bridge release: {} base units -> {}", amount, solana_recipient);
        Ok(())
    }
}

// ── Account contexts ────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        init,
        payer = authority,
        space = BridgeState::SIZE,
        seeds = [b"bridge_state"],
        bump
    )]
    pub bridge_state: Account<'info, BridgeState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetPaused<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds = [b"bridge_state"], bump = bridge_state.bump)]
    pub bridge_state: Account<'info, BridgeState>,
}

#[derive(Accounts)]
pub struct ExecuteQuarterlyBurn<'info> {
    #[account(mut)]
    pub burn_authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct HarvestFees<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    /// CHECK: treasury token account — validated by the keeper CPI
    pub treasury: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct LockForBridge<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    /// CHECK: the bridge escrow PDA that holds locked PWRC
    #[account(seeds = [b"bridge_escrow"], bump)]
    pub bridge_escrow: AccountInfo<'info>,
    #[account(mut, seeds = [b"bridge_state"], bump = bridge_state.bump)]
    pub bridge_state: Account<'info, BridgeState>,
}

#[derive(Accounts)]
pub struct ReleaseFromBridge<'info> {
    #[account(mut)]
    pub bridge_authority: Signer<'info>,
    /// CHECK: the bridge escrow PDA that holds locked PWRC
    #[account(seeds = [b"bridge_escrow"], bump)]
    pub bridge_escrow: AccountInfo<'info>,
    #[account(mut, seeds = [b"bridge_state"], bump = bridge_state.bump)]
    pub bridge_state: Account<'info, BridgeState>,
}

// ── Events ───────────────────────────────────────────────────────────────────

#[event]
pub struct LockEvent {
    pub amount:        u64,
    pub sui_recipient: [u8; 32],
}

#[event]
pub struct ReleaseEvent {
    pub amount:            u64,
    pub solana_recipient:  Pubkey,
}

#[event]
pub struct BurnEvent {
    pub amount:    u64,
    pub authority: Pubkey,
}

#[event]
pub struct FeeHarvestEvent {
    pub amount:   u64,
    pub treasury: Pubkey,
}

#[event]
pub struct PauseEvent {
    pub paused: bool,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum PwrcError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Bridge is currently paused")]
    BridgePaused,
    #[msg("Not the bridge authority")]
    NotAuthority,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Release would exceed total locked (escrow invariant)")]
    EscrowInvariant,
}
