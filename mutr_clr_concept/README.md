# MUTR CLR Concept (MutantRolls)

This folder contains a **conceptual Solana/Anchor design** for the  
MutantRolls (MUTR) **Combined Liquidity Reserve (CLR)** smart contract.

The CLR is planned to act as the central economic engine behind the game:

- Players can **stake MUTR** into the CLR and receive **xMUTR** share tokens.
- The CLR vault holds pooled MUTR and behaves like a **bankroll**.
- Games will pay prizes and receive liquidity directly from this vault.
- A separate **dividend pool** lets long-term stakers earn a share of CLR profits.

The core mechanics implemented in `mutr_clr.rs` (conceptually):

- `initialize_clr` – set up global state, mints and CLR vault.
- `stake` – deposit MUTR, mint xMUTR according to pool ratio, apply stake fee.
- `unstake` – burn xMUTR, withdraw MUTR from CLR, apply unstake fee.
- `join_dividend_pool` / `leave_dividend_pool` – move xMUTR into/out of the
  dividend pool (with a 4% exit fee on shares).
- `record_profit` – register new profit in the CLR and update reward-per-share.
- `claim_rewards` – claim accumulated MUTR dividends from the CLR vault.
- `send_prize` – pay prizes from the CLR to game winners (for approved games).

> ⚠️ **Important**
>
> This code is **not audited, not tested, and not intended for deployment**
> in its current form. It is a design reference for future development and
> for the MutantRolls community to review, discuss and improve.

Future work will include:

- Finalising PDA seeds and account constraints.
- Integrating on-chain randomness / VRF.
- Adding buyback logic tied to CLR thresholds.
- Implementing an approved-games registry with timelocks.
- Writing tests, audits and production-grade deployment scripts.
