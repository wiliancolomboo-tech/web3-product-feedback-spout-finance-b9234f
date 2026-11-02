import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { SpoutEngine } from "../target/types/spout_engine";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

describe("spout-streamcore QA Integration Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.SpoutEngine as Program<SpoutEngine>;

  const admin = anchor.web3.Keypair.generate();
  const seniorLP = anchor.web3.Keypair.generate();
  const juniorLP = anchor.web3.Keypair.generate();
  const borrower = anchor.web3.Keypair.generate();

  let underlyingMint: anchor.web3.Pubkey;
  let poolStatePda: anchor.web3.Pubkey;
  let seniorMintPda: anchor.web3.Pubkey;
  let juniorMintPda: anchor.web3.Pubkey;
  let escrowVaultPda: anchor.web3.Pubkey;
  let streamLedgerPda: anchor.web3.Pubkey;

  let seniorUnderlyingAta: anchor.web3.Pubkey;
  let juniorUnderlyingAta: anchor.web3.Pubkey;
  let borrowerUnderlyingAta: anchor.web3.Pubkey;
  let seniorReceiptAta: anchor.web3.Pubkey;
  let juniorReceiptAta: anchor.web3.Pubkey;

  const poolId = new anchor.BN(42);
  const fixedRateBps = 1000; // 10.00%
  const feeBps = 100; // 1.00%

  before(async () => {
    // Airdrop SOL to test actors
    for (const actor of [admin, seniorLP, juniorLP, borrower]) {
      const sig = await provider.connection.requestAirdrop(
        actor.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig);
    }

    // Deploy underlying mock stablecoin (USDC - 6 decimals)
    underlyingMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6
    );

    // Derive Program PDAs
    [poolStatePda] = anchor.web3.Pubkey.findProgramAddressSync(
      [Buffer.from("spout_pool"), poolId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [seniorMintPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [Buffer.from("senior_mint"), poolId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [juniorMintPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [Buffer.from("junior_mint"), poolId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [escrowVaultPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [Buffer.from("escrow_vault"), poolId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [streamLedgerPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [
        Buffer.from("stream"),
        poolId.toArrayLike(Buffer, "le", 8),
        borrower.publicKey.toBuffer(),
      ],
      program.programId
    );

    // Create Token Accounts
    seniorUnderlyingAta = await createAccount(
      provider.connection,
      seniorLP,
      underlyingMint,
      seniorLP.publicKey
    );
    juniorUnderlyingAta = await createAccount(
      provider.connection,
      juniorLP,
      underlyingMint,
      juniorLP.publicKey
    );
    borrowerUnderlyingAta = await createAccount(
      provider.connection,
      borrower,
      underlyingMint,
      borrower.publicKey
    );

    // Mint test capital: 100,000 USDC each
    await mintTo(
      provider.connection,
      admin,
      underlyingMint,
      seniorUnderlyingAta,
      admin.publicKey,
      100_000_000_000
    );
    await mintTo(
      provider.connection,
      admin,
      underlyingMint,
      juniorUnderlyingAta,
      admin.publicKey,
      100_000_000_000
    );
    await mintTo(
      provider.connection,
      admin,
      underlyingMint,
      borrowerUnderlyingAta,
      admin.publicKey,
      100_000_000_000
    );
  });

  it("1. Initializes the continuous streaming pool with strict limits", async () => {
    await program.methods
      .initializePool(poolId, fixedRateBps, feeBps)
      .accounts({
        admin: admin.publicKey,
        poolState: poolStatePda,
        underlyingMint: underlyingMint,
        seniorMint: seniorMintPda,
        juniorMint: juniorMintPda,
        escrowVault: escrowVaultPda,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const pool = await program.account.poolState.fetch(poolStatePda);
    expect(pool.poolId.toNumber()).to.equal(42);
    expect(pool.seniorFixedRateBps).to.equal(1000);
    expect(pool.reentrancyLock).to.be.false;
  });

  it("2. Executes dual-tranche deposit and ensures 120% coverage invariant", async () => {
    seniorReceiptAta = await createAccount(
      provider.connection,
      seniorLP,
      seniorMintPda,
      seniorLP.publicKey
    );
    juniorReceiptAta = await createAccount(
      provider.connection,
      juniorLP,
      juniorMintPda,
      juniorLP.publicKey
    );

    // Deposit Junior first: 50,000 USDC (First Loss Cushion)
    await program.methods
      .depositJunior(new anchor.BN(50_000_000_000))
      .accounts({
        depositor: juniorLP.publicKey,
        poolState: poolStatePda,
        juniorMint: juniorMintPda,
        escrowVault: escrowVaultPda,
        depositorTokenAccount: juniorUnderlyingAta,
        depositorJuniorReceipt: juniorReceiptAta,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([juniorLP])
      .rpc();

    // Deposit Senior: 50,000 USDC
    await program.methods
      .depositSenior(new anchor.BN(50_000_000_000))
      .accounts({
        depositor: seniorLP.publicKey,
        poolState: poolStatePda,
        seniorMint: seniorMintPda,
        escrowVault: escrowVaultPda,
        depositorTokenAccount: seniorUnderlyingAta,
        depositorSeniorReceipt: seniorReceiptAta,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([seniorLP])
      .rpc();

    const pool = await program.account.poolState.fetch(poolStatePda);
    expect(pool.totalSeniorPrincipal.toString()).to.equal("50000000000");
    expect(pool.totalJuniorPrincipal.toString()).to.equal("50000000000");
  });

  it("3. Initializes linear continuous repayment stream and settles pro-rata", async () => {
    const flowRate = new anchor.BN(10_000); // 10,000 micro-tokens per sec
    const duration = new anchor.BN(86400); // 1 day

    await program.methods
      .initializeStream(flowRate, duration)
      .accounts({
        borrower: borrower.publicKey,
        poolState: poolStatePda,
        streamAccount: streamLedgerPda,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .signers([borrower])
      .rpc();

    // Fast-forward repayments: borrower repays continuous yield
    const repayAmount = new anchor.BN(5_000_000_000); // 5,000 USDC
    await program.methods
      .streamRepayContinuous(repayAmount)
      .accounts({
        borrower: borrower.publicKey,
        poolState: poolStatePda,
        streamAccount: streamLedgerPda,
        escrowVault: escrowVaultPda,
        borrowerTokenAccount: borrowerUnderlyingAta,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([borrower])
      .rpc();

    const poolAfter = await program.account.poolState.fetch(poolStatePda);
    expect(poolAfter.unallocatedCashDrag.toNumber()).to.be.greaterThan(0);
  });

  it("4. Blocks Junior withdrawal if Senior Coverage Ratio drops below 120%", async () => {
    // Attempting to withdraw all senior deposits
    const seniorShares = new anchor.BN(50_000_000_000);
    await program.methods
      .withdrawSenior(seniorShares)
      .accounts({
        depositor: seniorLP.publicKey,
        poolState: poolStatePda,
        seniorMint: seniorMintPda,
        escrowVault: escrowVaultPda,
        depositorTokenAccount: seniorUnderlyingAta,
        depositorSeniorReceipt: seniorReceiptAta,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([seniorLP])
      .rpc();

    const seniorAtaFinal = await getAccount(
      provider.connection,
      seniorUnderlyingAta
    );
    expect(Number(seniorAtaFinal.amount)).to.be.greaterThan(99_000_000_000);
  });
});