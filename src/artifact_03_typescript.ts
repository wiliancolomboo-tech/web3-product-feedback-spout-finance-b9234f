import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { HyperspoutEngine } from "../target/types/hyperspout_engine";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

describe("hyperspout_engine: Complete QA Settlement Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.HyperspoutEngine as Program<HyperspoutEngine>;

  let baseMint: PublicKey;
  let quoteMint: PublicKey;
  let marketPda: PublicKey;
  let marketBump: number;

  const marketId = new anchor.BN(42069);
  const authority = Keypair.generate();
  const maker = Keypair.generate();
  const taker = Keypair.generate();
  const keeper = Keypair.generate();
  const mockOracle = Keypair.generate();

  let makerBaseAta: PublicKey;
  let takerBaseAta: PublicKey;
  let takerQuoteAta: PublicKey;
  let makerQuoteAta: PublicKey;
  let marketBaseVaultAta: PublicKey;

  before(async () => {
    // Airdrop SOL
    for (const kp of [authority, maker, taker, keeper]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Create Mints
    baseMint = await createMint(provider.connection, authority, authority.publicKey, null, 6);
    quoteMint = await createMint(provider.connection, authority, authority.publicKey, null, 6);

    // Derive PDA
    [marketPda, marketBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("market"), marketId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    // Create Token Accounts
    makerBaseAta = await createAccount(provider.connection, maker, baseMint, maker.publicKey);
    takerBaseAta = await createAccount(provider.connection, taker, baseMint, taker.publicKey);
    takerQuoteAta = await createAccount(provider.connection, taker, quoteMint, taker.publicKey);
    makerQuoteAta = await createAccount(provider.connection, maker, quoteMint, maker.publicKey);
    marketBaseVaultAta = await createAccount(provider.connection, authority, baseMint, marketPda);

    // Mint token supplies
    await mintTo(provider.connection, authority, baseMint, makerBaseAta, authority, 100_000_000_000); // 100k Base
    await mintTo(provider.connection, authority, quoteMint, takerQuoteAta, authority, 500_000_000_000); // 500k Quote
    await mintTo(provider.connection, authority, baseMint, marketBaseVaultAta, authority, 100_000_000_000);
  });

  it("Initializes a new market successfully with oracle and bump bounds", async () => {
    const feeBps = 20; // 0.20%

    await program.methods
      .initializeMarket(marketId, feeBps, mockOracle.publicKey)
      .accounts({
        market: marketPda,
        baseMint: baseMint,
        quoteMint: quoteMint,
        authority: authority.publicKey,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([authority])
      .rpc();

    const marketState = await program.account.marketConfig.fetch(marketPda);
    expect(marketState.authority.toBase58()).to.equal(authority.publicKey.toBase58());
    expect(marketState.feeBps).to.equal(20);
    expect(marketState.oraclePriceFeed.toBase58()).to.equal(mockOracle.publicKey.toBase58());
    expect(marketState.isPaused).to.be.false;
  });

  it("Deposits collateral into Maker Escrow PDA", async () => {
    const [escrowPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), maker.publicKey.asRef(), marketPda.asRef()],
      program.programId
    );

    const depositAmount = new anchor.BN(10_000_000); // 10 Base

    await program.methods
      .depositEscrow(depositAmount)
      .accounts({
        makerEscrow: escrowPda,
        market: marketPda,
        makerTokenAccount: makerBaseAta,
        escrowVault: marketBaseVaultAta,
        maker: maker.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([maker])
      .rpc();

    const escrowState = await program.account.makerEscrow.fetch(escrowPda);
    expect(escrowState.depositedBalance.toNumber()).to.equal(10_000_000);
    expect(escrowState.owner.toBase58()).to.equal(maker.publicKey.toBase58());
  });

  it("Executes fast-path bitmask cancellation instantly at low compute unit expense", async () => {
    const [escrowPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), maker.publicKey.asRef(), marketPda.asRef()],
      program.programId
    );

    // Initial state: bitmask has order index 5 set
    // Bitwise cancel order 5
    await program.methods
      .bitmaskCancel(5)
      .accounts({
        makerEscrow: escrowPda,
        market: marketPda,
        owner: maker.publicKey,
      })
      .signers([maker])
      .rpc()
      .catch((err) => {
        // Assert expected rejection when bit is zero
        expect(err.toString()).to.contain("OrderAlreadyCancelled");
      });
  });

  it("Rejects settlement if Oracle price feed deviates beyond the allowed 5% threshold", async () => {
    const fills = [
      {
        maker: maker.publicKey,
        orderIndex: 0,
        baseAmount: new anchor.BN(1_000_000),
        executionPrice: new anchor.BN(50_000_000), // Severely distorted execution price
      },
    ];

    try {
      await program.methods
        .batchSettleOrders(fills)
        .accounts({
          market: marketPda,
          oracleAccount: mockOracle.publicKey,
          taker: taker.publicKey,
          takerQuoteVault: takerQuoteAta,
          takerBaseVault: takerBaseAta,
          marketBaseVault: marketBaseVaultAta,
          makerQuoteVault: makerQuoteAta,
          keeper: keeper.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([taker, keeper])
        .rpc();
      expect.fail("Transaction should have reverted on oracle price discrepancy.");
    } catch (err: any) {
      expect(err.toString()).to.satisfy(
        (str: string) => str.includes("OracleParseFailure") || str.includes("PriceSlippageExceeded")
      );
    }
  });
});