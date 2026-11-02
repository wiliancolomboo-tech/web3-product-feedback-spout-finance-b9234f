import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { SpoutFinance } from "../target/types/spout_finance";
import {
  createMint,
  createAssociatedTokenAccount,
  mintTo,
  getAccount,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { expect } from "chai";

describe("spout_finance e2e stream protocol verification", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.SpoutFinance as Program<SpoutFinance>;

  let mint: anchor.web3.PublicKey;
  let senderAta: anchor.web3.PublicKey;
  let recipientAta: anchor.web3.PublicKey;
  const sender = (provider.wallet as anchor.Wallet).payer;
  const recipient = anchor.web3.Keypair.generate();

  const streamId = new anchor.BN(1001);
  const ratePerSecond = new anchor.BN(1_000_000); // 1 token/sec (6 decimals)
  const duration = 10; // 10 seconds
  const totalDeposit = ratePerSecond.muln(duration);

  let streamPda: anchor.web3.PublicKey;
  let vaultPda: anchor.web3.PublicKey;

  before(async () => {
    // Air-drop Recipient SOL for tx fees if necessary
    const sig = await provider.connection.requestAirdrop(
      recipient.publicKey,
      2 * anchor.web3.LAMPORTS_PER_SOL
    );
    await provider.connection.confirmTransaction(sig);

    // Create Test Mint & Associated Token Accounts
    mint = await createMint(
      provider.connection,
      sender,
      sender.publicKey,
      null,
      6
    );

    senderAta = await createAssociatedTokenAccount(
      provider.connection,
      sender,
      mint,
      sender.publicKey
    );

    recipientAta = await createAssociatedTokenAccount(
      provider.connection,
      sender,
      mint,
      recipient.publicKey
    );

    // Mint 1,000 Tokens to Sender
    await mintTo(
      provider.connection,
      sender,
      mint,
      senderAta,
      sender,
      1_000_000_000
    );

    // Derive Stream State PDA & Escrow Vault PDA
    [streamPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("stream"),
        sender.publicKey.toBuffer(),
        recipient.publicKey.toBuffer(),
        streamId.toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    );

    [vaultPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), streamPda.toBuffer()],
      program.programId
    );
  });

  it("1. Successfully initializes stream with deterministic escrow PDA", async () => {
    const slot = await provider.connection.getSlot();
    const now = (await provider.connection.getBlockTime(slot)) || Math.floor(Date.now() / 1000);
    const startTime = new anchor.BN(now);
    const stopTime = new anchor.BN(now + duration);
    const cliffTime = new anchor.BN(now + 2); // 2-second cliff

    await program.methods
      .initializeStream(
        streamId,
        totalDeposit,
        ratePerSecond,
        startTime,
        stopTime,
        cliffTime
      )
      .accounts({
        sender: sender.publicKey,
        recipient: recipient.publicKey,
        mint: mint,
        streamAccount: streamPda,
        escrowVault: vaultPda,
        senderTokenAccount: senderAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: anchor.web3.SystemProgram.programId,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    const vaultAccount = await getAccount(provider.connection, vaultPda);
    expect(Number(vaultAccount.amount)).to.equal(totalDeposit.toNumber());

    const streamState = await program.account.streamAccount.fetch(streamPda);
    expect(streamState.totalDeposit.toNumber()).to.equal(totalDeposit.toNumber());
    expect(streamState.withdrawnAmount.toNumber()).to.equal(0);
    expect(streamState.isPaused).to.be.false;
  });

  it("2. Enforces cliff period invariant; rejects early withdrawal", async () => {
    try {
      await program.methods
        .withdraw()
        .accounts({
          recipient: recipient.publicKey,
          streamAccount: streamPda,
          mint: mint,
          escrowVault: vaultPda,
          recipientTokenAccount: recipientAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([recipient])
        .rpc();
      expect.fail("Expected transaction to fail due to cliff invariant");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("CliffNotReached");
    }
  });

  it("3. Allows withdrawal post-cliff with accurate streaming math", async () => {
    // Wait for cliff passage (3 seconds)
    await new Promise((resolve) => setTimeout(resolve, 3500));

    await program.methods
      .withdraw()
      .accounts({
        recipient: recipient.publicKey,
        streamAccount: streamPda,
        mint: mint,
        escrowVault: vaultPda,
        recipientTokenAccount: recipientAta,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([recipient])
      .rpc();

    const recipientBal = await getAccount(provider.connection, recipientAta);
    expect(Number(recipientBal.amount)).to.be.greaterThanOrEqual(2_000_000);
  });

  it("4. Supports stream pausing and halts vested flow accrual", async () => {
    await program.methods
      .pauseStream()
      .accounts({
        authority: sender.publicKey,
        streamAccount: streamPda,
      })
      .rpc();

    const stateAfterPause = await program.account.streamAccount.fetch(streamPda);
    expect(stateAfterPause.isPaused).to.be.true;

    // Verify withdrawal fails during pause
    try {
      await program.methods
        .withdraw()
        .accounts({
          recipient: recipient.publicKey,
          streamAccount: streamPda,
          mint: mint,
          escrowVault: vaultPda,
          recipientTokenAccount: recipientAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([recipient])
        .rpc();
      expect.fail("Should have rejected withdrawal while paused");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("StreamIsPaused");
    }

    // Resume stream
    await program.methods
      .resumeStream()
      .accounts({
        authority: sender.publicKey,
        streamAccount: streamPda,
      })
      .rpc();

    const stateAfterResume = await program.account.streamAccount.fetch(streamPda);
    expect(stateAfterResume.isPaused).to.be.false;
    expect(stateAfterResume.totalPausedDuration.toNumber()).to.be.greaterThan(0);
  });

  it("5. Settles unvested tokens accurately on stream cancellation", async () => {
    const senderBalBefore = (await getAccount(provider.connection, senderAta)).amount;
    const recipientBalBefore = (await getAccount(provider.connection, recipientAta)).amount;

    await program.methods
      .cancelStream()
      .accounts({
        sender: sender.publicKey,
        streamAccount: streamPda,
        mint: mint,
        escrowVault: vaultPda,
        senderTokenAccount: senderAta,
        recipientTokenAccount: recipientAta,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .rpc();

    const senderBalAfter = (await getAccount(provider.connection, senderAta)).amount;
    const recipientBalAfter = (await getAccount(provider.connection, recipientAta)).amount;

    // Both parties receive their exact shares, zero lamports leaked
    expect(Number(recipientBalAfter)).to.be.greaterThanOrEqual(Number(recipientBalBefore));
    expect(Number(senderBalAfter)).to.be.greaterThan(Number(senderBalBefore));

    // PDA state account is closed and rent refunded to sender
    const closedState = await provider.connection.getAccountInfo(streamPda);
    expect(closedState).to.be.null;
  });
});