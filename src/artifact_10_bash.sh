# Set Solana CLI target to Devnet
solana config set --url https://api.devnet.solana.com

# Ensure deployer wallet is funded
solana airdrop 2

# Deploy program
anchor deploy --provider.cluster devnet

# Verify program deployment
solana program show <PROGRAM_ID>