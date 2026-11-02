# 1. Clone the project and navigate into workspace
git clone https://github.com/spout-finance/spout-engine.git
cd spout-engine

# 2. Install Node dependencies
yarn install

# 3. Synchronize Program ID
anchor keys sync

# 4. Build the Anchor Rust program (Generates IDL & target artifacts)
anchor build

# 5. Run the Localnet Test Matrix
anchor test