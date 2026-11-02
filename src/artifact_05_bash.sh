# 1. Clone repository and change directory
git clone https://github.com/spout-finance/spout-streamcore.git
cd spout-streamcore

# 2. Install TypeScript dependencies
npm install

# 3. Clean and build Anchor Program with maximum release optimizations
anchor build

# 4. Extract generated Program ID and verify keys
solana address -k target/deploy/spout_engine-keypair.json

# 5. Run the comprehensive QA test matrix against local Solana test validator
anchor test