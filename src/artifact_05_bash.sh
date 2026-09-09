# 1. Clone repository and navigate to root
git clone https://github.com/spout-finance-feedback/hyperspout.git
cd hyperspout

# 2. Install TypeScript dependencies
yarn install

# 3. Synchronize Anchor program address
anchor keys sync

# 4. Build the Anchor Rust program with optimized release profiles
cargo build-sbf
anchor build