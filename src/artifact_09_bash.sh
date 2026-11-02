# Set network config to Devnet
solana config set --url https://api.devnet.solana.com

# Ensure sufficient SOL for deployment
solana airdrop 2

# Deploy the verified program binary
anchor deploy --provider.cluster devnet

# Verify deployment
solana program show SpoutGuard11111111111111111111111111111111